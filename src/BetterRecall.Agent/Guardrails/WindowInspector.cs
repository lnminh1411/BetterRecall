// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Runtime.InteropServices;
using BetterRecall.Agent.Native;
using BetterRecall.Contracts;

namespace BetterRecall.Agent.Guardrails;

internal sealed record MonitorTop(int Monitor, WindowInfo? Window, bool DeniedVisible);

/// <summary>Everything the capture path needs from one scan of the window list.</summary>
internal sealed record GuardSnapshot(
    long TakenAtMs,
    WindowInfo? Foreground,
    GuardDecision Decision,
    BadgeResult Badge,
    bool AnyAffinity,
    RECT[] MaskRects,
    IReadOnlyDictionary<int, MonitorTop> PerMonitor,
    string Site = "")         // host in the foreground browser's address bar (privacy.recordSite), else empty
{
    public static readonly GuardSnapshot Closed = new(0, null, GuardDecision.Deny(GuardReason.NoForeground), BadgeResult.NotChecked, false, [], new Dictionary<int, MonitorTop>());
}

/// <summary>Win32 window enumeration for the guardrails. Called from the guardrail thread only.</summary>
internal static unsafe class WindowInspector
{
    [ThreadStatic] private static List<nint>? _collect;
    private static readonly Dictionary<uint, (string Path, long At)> PathCache = new();
    private static readonly HashSet<string> ShellClasses = new(StringComparer.Ordinal)
    {
        "Progman", "WorkerW", "Shell_TrayWnd", "Shell_SecondaryTrayWnd", "Windows.UI.Core.CoreWindow", "NotifyIconOverflowWindow",
    };

    public static WindowInfo? Describe(nint hwnd)
    {
        if (hwnd == 0) return null;
        nint root = Win32.GetAncestor(hwnd, Win32.GA_ROOT);
        if (root != 0) hwnd = root;
        uint pid;
        Win32.GetWindowThreadProcessId(hwnd, &pid);
        return new WindowInfo(hwnd, pid, ProcessPath(pid), Win32.GetWindowText(hwnd), Win32.GetClassName(hwnd));
    }

    public static WindowInfo? Foreground() => Describe(Win32.GetForegroundWindow());

    public static string ProcessPath(uint pid)
    {
        long now = Environment.TickCount64;
        lock (PathCache)
        {
            if (PathCache.TryGetValue(pid, out var c) && now - c.At < 30_000) return c.Path;
        }
        var path = Win32.GetProcessImagePath(pid) ?? "";
        lock (PathCache)
        {
            if (PathCache.Count > 512) PathCache.Clear();
            PathCache[pid] = (path, now);
        }
        return path;
    }

    [UnmanagedCallersOnly]
    private static int Collect(nint hwnd, nint _)
    {
        _collect!.Add(hwnd);
        return 1;
    }

    /// <summary>Top-level windows in z-order, topmost first.</summary>
    public static List<nint> TopLevelWindows()
    {
        _collect = new List<nint>(256);
        Win32.EnumWindows(&Collect, 0);
        var list = _collect;
        _collect = null;
        return list;
    }

    public static bool TryGetBounds(nint hwnd, out RECT r)
    {
        RECT rect;
        if (Win32.DwmGetWindowAttribute(hwnd, Win32.DWMWA_EXTENDED_FRAME_BOUNDS, &rect, (uint)sizeof(RECT)) == 0 || Win32.GetWindowRect(hwnd, &rect))
        {
            r = rect;
            return r.Width > 0 && r.Height > 0;
        }
        r = default;
        return false;
    }

    /// <summary>
    /// One pass over visible top-level windows: display affinity (ignoring own windows), deny-list masks,
    /// and the topmost window per monitor for keyframe titles.
    /// </summary>
    public static (bool AnyAffinity, RECT[] Masks, Dictionary<int, MonitorTop> PerMonitor) Scan(
        PrivacySettings p, string installDir, uint ownPid, IReadOnlyList<(int Index, RECT Bounds)> monitors)
    {
        bool anyAffinity = false;
        var masks = new List<RECT>();
        var tops = new Dictionary<int, WindowInfo?>();
        var denied = new Dictionary<int, bool>();
        foreach (var hwnd in TopLevelWindows())
        {
            if (!Win32.IsWindowVisible(hwnd) || Win32.IsIconic(hwnd) || Win32.IsCloaked(hwnd)) continue;
            if (!TryGetBounds(hwnd, out var r)) continue;
            uint pid;
            Win32.GetWindowThreadProcessId(hwnd, &pid);
            var exe = ProcessPath(pid);
            bool own = pid == ownPid || (installDir.Length > 0 && exe.StartsWith(installDir, StringComparison.OrdinalIgnoreCase));
            uint aff = 0;
            if (!own && Win32.GetWindowDisplayAffinity(hwnd, &aff) && aff != Win32.WDA_NONE) anyAffinity = true;
            var cls = Win32.GetClassName(hwnd);
            if (ShellClasses.Contains(cls)) continue;
            var title = Win32.GetWindowText(hwnd);
            var info = new WindowInfo(hwnd, pid, exe, title, cls);
            bool deny = GuardrailPolicy.MatchDenyLists(info, p) != GuardReason.None;
            if (deny) masks.Add(r);
            long exStyle = Win32.GetWindowLongPtrW(hwnd, Win32.GWL_EXSTYLE);
            bool tool = (exStyle & Win32.WS_EX_TOOLWINDOW) != 0;
            foreach (var (index, mb) in monitors)
            {
                if (!Intersects(r, mb)) continue;
                if (deny) denied[index] = true;
                if (!tops.ContainsKey(index) && !tool && !own && title.Length > 0 && Covers(r, mb)) tops[index] = info;
            }
        }
        var per = new Dictionary<int, MonitorTop>();
        foreach (var (index, _) in monitors)
            per[index] = new MonitorTop(index, tops.GetValueOrDefault(index), denied.GetValueOrDefault(index));
        return (anyAffinity, masks.ToArray(), per);
    }

    private static bool Intersects(RECT a, RECT b) => a.Left < b.Right && b.Left < a.Right && a.Top < b.Bottom && b.Top < a.Bottom;

    /// <summary>Window covers the monitor's centre point.</summary>
    private static bool Covers(RECT w, RECT m)
    {
        int cx = (m.Left + m.Right) / 2, cy = (m.Top + m.Bottom) / 2;
        return w.Left <= cx && cx < w.Right && w.Top <= cy && cy < w.Bottom;
    }
}
