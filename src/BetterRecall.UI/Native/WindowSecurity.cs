// SPDX-License-Identifier: MIT
// Copyright (c) BetterRecall contributors
using System.Runtime.InteropServices;

namespace BetterRecall.UI.Native;

/// <summary>
/// security.excludeOwnWindows: WDA_EXCLUDEFROMCAPTURE on every top-level window of this process.
/// Windows created by XAML or the system for us (pickers, popup hosts, tooltips) are caught by a WinEvent hook.
/// </summary>
internal static partial class WindowSecurity
{
    private const uint WDA_NONE = 0x0;
    private const uint WDA_EXCLUDEFROMCAPTURE = 0x11;

    private const uint EVENT_OBJECT_SHOW = 0x8002;
    private const uint WINEVENT_OUTOFCONTEXT = 0x0;
    private const uint GA_ROOT = 2;
    private const uint DWMWA_CLOAKED = 14;

    [LibraryImport("user32.dll")]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static partial bool IsWindowVisible(nint hwnd);

    [LibraryImport("dwmapi.dll")]
    private static partial int DwmGetWindowAttribute(nint hwnd, uint attribute, ref int value, int size);

    private delegate void WinEventProc(nint hook, uint ev, nint hwnd, int idObject, int idChild, uint thread, uint time);
    private delegate bool EnumProc(nint hwnd, nint lParam);

    [LibraryImport("user32.dll", SetLastError = true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static partial bool SetWindowDisplayAffinity(nint hwnd, uint affinity);

    [LibraryImport("user32.dll")]
    private static partial nint SetWinEventHook(uint min, uint max, nint module, WinEventProc proc, uint pid, uint tid, uint flags);

    [LibraryImport("user32.dll")]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static partial bool UnhookWinEvent(nint hook);

    [LibraryImport("user32.dll")]
    private static partial nint GetAncestor(nint hwnd, uint flags);

    [LibraryImport("user32.dll")]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static partial bool EnumWindows(EnumProc proc, nint lParam);

    [LibraryImport("user32.dll")]
    private static partial uint GetWindowThreadProcessId(nint hwnd, out uint pid);

    private static readonly uint OwnPid = (uint)Environment.ProcessId;
    private static WinEventProc? _proc;
    private static nint _hook;

    public static bool Exclude { get; private set; } = true;

    /// <summary>Call once on the UI thread (the hook delivers on the thread that set it).</summary>
    public static void Start()
    {
        if (_hook != 0) return;
        _proc = OnWinEvent;
        // SHOW only: EVENT_OBJECT_CREATE fires for windows that stay hidden (popup hosts, tooltip windows), and
        // changing their affinity while hidden made Windows draw an empty white topmost rectangle.
        _hook = SetWinEventHook(EVENT_OBJECT_SHOW, EVENT_OBJECT_SHOW, 0, _proc, OwnPid, 0, WINEVENT_OUTOFCONTEXT);
        ApplyAll();
    }

    public static void Stop()
    {
        if (_hook != 0) UnhookWinEvent(_hook);
        _hook = 0;
    }

    /// <summary>Apply to one window we created. Child windows are ignored by the API, so only roots are passed.</summary>
    public static void Apply(nint hwnd)
    {
        if (hwnd == 0) return;
        uint want = Exclude ? WDA_EXCLUDEFROMCAPTURE : WDA_NONE;
        if (GetWindowDisplayAffinity(hwnd, out var current) && current == want) return;   // no needless redraw
        SetWindowDisplayAffinity(hwnd, want);
    }

    [LibraryImport("user32.dll")]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static partial bool GetWindowDisplayAffinity(nint hwnd, out uint affinity);

    /// <summary>Setting changed: re-apply to every top-level window of this process.</summary>
    public static void SetExclude(bool exclude)
    {
        Exclude = exclude;
        ApplyAll();
    }

    /// <summary>Visible windows only. Changing the affinity of a hidden or cloaked window (the warm launcher) made
    /// Windows draw it as an empty white topmost rectangle; hidden windows get the current value when shown
    /// (EVENT_OBJECT_SHOW below, and the windows call <see cref="Apply"/> before showing).</summary>
    public static void ApplyAll()
    {
        EnumWindows((h, _) =>
        {
            GetWindowThreadProcessId(h, out var pid);
            if (pid == OwnPid && IsWindowVisible(h) && !IsCloaked(h)) Apply(h);
            return true;
        }, 0);
    }

    private static bool IsCloaked(nint hwnd)
    {
        int cloaked = 0;
        return DwmGetWindowAttribute(hwnd, DWMWA_CLOAKED, ref cloaked, sizeof(int)) == 0 && cloaked != 0;
    }

    private static void OnWinEvent(nint hook, uint ev, nint hwnd, int idObject, int idChild, uint thread, uint time)
    {
        if (hwnd == 0 || idObject != 0 || idChild != 0) return;
        if (GetAncestor(hwnd, GA_ROOT) != hwnd || !IsWindowVisible(hwnd)) return;
        Apply(hwnd);
    }
}
