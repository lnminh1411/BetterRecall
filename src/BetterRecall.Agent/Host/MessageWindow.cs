// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Runtime.InteropServices;
using BetterRecall.Agent.Native;
using BetterRecall.Agent.Util;

namespace BetterRecall.Agent.Host;

internal enum TrayCommand
{
    Search = 1, Pause15 = 2, Pause60 = 3, PauseUntilResume = 4, Resume = 5, SaveReplay = 6, Panic = 7, Settings = 8, Quit = 9, Status = 10,
}

/// <summary>
/// Hidden message-only style window on the main STA thread: owner HWND for Windows Hello prompts,
/// RegisterHotKey target, tray icon callback, session lock notifications and the foreground WinEvent hook.
/// </summary>
internal sealed unsafe class MessageWindow : IDisposable
{
    private const uint WM_TRAY = Win32.WM_APP + 1;
    private const uint WM_INVOKE = Win32.WM_APP + 2;
    private static MessageWindow? _instance;
    private static readonly uint TaskbarCreated = Win32.RegisterWindowMessageW("TaskbarCreated");

    private readonly Queue<Action> _invokes = new();
    private nint _hwnd;
    private nint _winEventHook;
    private bool _iconAdded;
    private string _tip = "BetterRecall";
    private readonly Dictionary<int, string> _hotkeyNames = new();

    public nint Handle => _hwnd;
    public event Action<string>? Hotkey;
    public event Action<TrayCommand>? Command;
    public event Action? BalloonClicked;
    public event Action<int>? SessionChanged;
    public event Action? DisplayChanged;
    public event Action? ForegroundChanged;
    public Func<(string Status, bool Paused, bool ReplayOn)>? MenuState;

    public MessageWindow()
    {
        _instance = this;
        var cls = "BetterRecall.Agent.Window";
        fixed (char* name = cls)
        {
            var wc = new WNDCLASSEXW
            {
                cbSize = (uint)sizeof(WNDCLASSEXW),
                lpfnWndProc = &WndProc,
                hInstance = Win32.GetModuleHandleW(null),
                lpszClassName = name,
            };
            if (Win32.RegisterClassExW(&wc) == 0) throw new InvalidOperationException($"RegisterClassEx failed ({Marshal.GetLastPInvokeError()}).");
            // A top-level hidden window (not HWND_MESSAGE): it receives broadcast messages such as TaskbarCreated and WM_DISPLAYCHANGE.
            _hwnd = Win32.CreateWindowExW(0, name, name, 0, 0, 0, 0, 0, 0, 0, wc.hInstance, 0);
        }
        if (_hwnd == 0) throw new InvalidOperationException("CreateWindowEx failed.");
        Win32.WTSRegisterSessionNotification(_hwnd, 0 /* NOTIFY_FOR_THIS_SESSION */);
        _winEventHook = Win32.SetWinEventHook(Win32.EVENT_SYSTEM_FOREGROUND, Win32.EVENT_SYSTEM_FOREGROUND, 0, &OnWinEvent, 0, 0,
            Win32.WINEVENT_OUTOFCONTEXT | Win32.WINEVENT_SKIPOWNPROCESS);
    }

    /// <summary>Runs the message loop until Quit.</summary>
    public void Run()
    {
        MSG msg;
        while (Win32.GetMessageW(&msg, 0, 0, 0) > 0)
        {
            Win32.TranslateMessage(&msg);
            Win32.DispatchMessageW(&msg);
        }
    }

    public void Quit() => Invoke(() => Win32.PostQuitMessage(0));

    /// <summary>Runs an action on the window thread.</summary>
    public void Invoke(Action a)
    {
        lock (_invokes) _invokes.Enqueue(a);
        Win32.PostMessageW(_hwnd, WM_INVOKE, 0, 0);
    }

    // ---- hotkeys ----

    public bool RegisterHotkey(int id, string name, Hotkey hk)
    {
        Win32.UnregisterHotKey(_hwnd, id);
        bool ok = Win32.RegisterHotKey(_hwnd, id, hk.Modifiers | Win32.MOD_NOREPEAT, hk.VirtualKey);
        if (ok) _hotkeyNames[id] = name;
        else Log.Warn($"hotkey {name} ({HotkeyParser.Format(hk)}) not registered: error {Marshal.GetLastPInvokeError()}");
        return ok;
    }

    public void UnregisterHotkey(int id)
    {
        Win32.UnregisterHotKey(_hwnd, id);
        _hotkeyNames.Remove(id);
    }

    // ---- tray ----

    public void ShowTray(string tip)
    {
        _tip = tip;
        var d = NewData();
        d.uFlags = Win32.NIF_MESSAGE | Win32.NIF_ICON | Win32.NIF_TIP | Win32.NIF_SHOWTIP;
        d.uCallbackMessage = WM_TRAY;
        d.hIcon = TrayIcon();
        SetText(d.szTip, 128, tip);
        if (!_iconAdded)
        {
            _iconAdded = Win32.Shell_NotifyIconW(Win32.NIM_ADD, &d);
            d.uVersion = Win32.NOTIFYICON_VERSION_4;
            Win32.Shell_NotifyIconW(Win32.NIM_SETVERSION, &d);
        }
        else Win32.Shell_NotifyIconW(Win32.NIM_MODIFY, &d);
    }

    private nint _trayIcon;
    private bool _trayLight;

    /// <summary>Clock glyph on transparent: black on a light taskbar, white on a dark one (icons\tray-*.ico next to the
    /// exe). Falls back to the generic application icon when the file is missing.</summary>
    private nint TrayIcon()
    {
        bool light = TaskbarIsLight();
        if (_trayIcon != 0 && light == _trayLight) return _trayIcon;
        int size = Win32.GetSystemMetrics(Win32.SM_CXSMICON);
        var path = Path.Combine(AppContext.BaseDirectory, "icons", light ? "tray-light.ico" : "tray-dark.ico");
        var icon = File.Exists(path) ? Win32.LoadImageW(0, path, Win32.IMAGE_ICON, size, size, Win32.LR_LOADFROMFILE) : 0;
        if (icon == 0) return Win32.LoadIconW(0, 32512 /* IDI_APPLICATION */);
        if (_trayIcon != 0) Win32.DestroyIcon(_trayIcon);
        _trayIcon = icon;
        _trayLight = light;
        return icon;
    }

    /// <summary>HKCU Personalize SystemUsesLightTheme (taskbar and tray colour); dark when unreadable.</summary>
    private static bool TaskbarIsLight()
    {
        try
        {
            using var k = Microsoft.Win32.Registry.CurrentUser.OpenSubKey(@"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize");
            return k?.GetValue("SystemUsesLightTheme") is int v && v != 0;
        }
        catch (Exception e) when (e is System.Security.SecurityException or IOException or UnauthorizedAccessException) { return false; }
    }

    /// <summary>Theme switch: swap the tray icon when the taskbar colour changed.</summary>
    private void RefreshTrayIcon()
    {
        if (!_iconAdded || TaskbarIsLight() == _trayLight) return;
        var d = NewData();
        d.uFlags = Win32.NIF_ICON;
        d.hIcon = TrayIcon();
        Win32.Shell_NotifyIconW(Win32.NIM_MODIFY, &d);
    }

    public void UpdateTip(string tip)
    {
        if (!_iconAdded || tip == _tip) return;
        _tip = tip;
        var d = NewData();
        d.uFlags = Win32.NIF_TIP | Win32.NIF_SHOWTIP;
        SetText(d.szTip, 128, tip);
        Win32.Shell_NotifyIconW(Win32.NIM_MODIFY, &d);
    }

    /// <summary>Tray balloon (shown as a Windows notification). A click raises BalloonClicked.</summary>
    public void Notify(string title, string text)
    {
        if (!_iconAdded) return;
        var d = NewData();
        d.uFlags = Win32.NIF_INFO;
        d.dwInfoFlags = Win32.NIIF_INFO | Win32.NIIF_NOSOUND;
        SetText(d.szInfoTitle, 64, title);
        SetText(d.szInfo, 256, text);
        Win32.Shell_NotifyIconW(Win32.NIM_MODIFY, &d);
    }

    private NOTIFYICONDATAW NewData() => new() { cbSize = (uint)sizeof(NOTIFYICONDATAW), hWnd = _hwnd, uID = 1 };

    private static void SetText(char* dst, int max, string s)
    {
        int n = Math.Min(s.Length, max - 1);
        for (int i = 0; i < n; i++) dst[i] = s[i];
        dst[n] = '\0';
    }

    private void ShowMenu()
    {
        var state = MenuState?.Invoke() ?? ("BetterRecall", false, false);
        nint menu = Win32.CreatePopupMenu();
        try
        {
            Win32.AppendMenuW(menu, Win32.MF_STRING | Win32.MF_GRAYED, (nuint)TrayCommand.Status, state.Status);
            Win32.AppendMenuW(menu, Win32.MF_SEPARATOR, 0, null);
            Win32.AppendMenuW(menu, Win32.MF_STRING, (nuint)TrayCommand.Search, "Search");
            if (state.Paused) Win32.AppendMenuW(menu, Win32.MF_STRING, (nuint)TrayCommand.Resume, "Resume capture");
            Win32.AppendMenuW(menu, Win32.MF_STRING, (nuint)TrayCommand.Pause15, "Pause for 15 minutes");
            Win32.AppendMenuW(menu, Win32.MF_STRING, (nuint)TrayCommand.Pause60, "Pause for 1 hour");
            Win32.AppendMenuW(menu, Win32.MF_STRING, (nuint)TrayCommand.PauseUntilResume, "Pause until resumed");
            Win32.AppendMenuW(menu, Win32.MF_STRING | (state.ReplayOn ? 0 : Win32.MF_GRAYED), (nuint)TrayCommand.SaveReplay, "Save replay");
            Win32.AppendMenuW(menu, Win32.MF_STRING, (nuint)TrayCommand.Panic, "Panic: flush memory now");
            Win32.AppendMenuW(menu, Win32.MF_SEPARATOR, 0, null);
            Win32.AppendMenuW(menu, Win32.MF_STRING, (nuint)TrayCommand.Settings, "Settings");
            Win32.AppendMenuW(menu, Win32.MF_STRING, (nuint)TrayCommand.Quit, "Quit BetterRecall");
            POINT pt;
            Win32.GetCursorPos(&pt);
            Win32.SetForegroundWindow(_hwnd);
            int cmd = Win32.TrackPopupMenuEx(menu, Win32.TPM_RIGHTBUTTON | Win32.TPM_RETURNCMD | Win32.TPM_NONOTIFY, pt.X, pt.Y, _hwnd, 0);
            if (cmd > 0) Raise(() => Command?.Invoke((TrayCommand)cmd));
        }
        finally { Win32.DestroyMenu(menu); }
    }

    private static void Raise(Action a)
    {
        try { a(); }
        catch (Exception ex) { Log.Error("window event handler", ex); }
    }

    [UnmanagedCallersOnly]
    private static void OnWinEvent(nint hook, uint ev, nint hwnd, int idObject, int idChild, uint thread, uint time)
    {
        var w = _instance;
        if (w is null || idObject != 0) return;
        Raise(() => w.ForegroundChanged?.Invoke());
    }

    [UnmanagedCallersOnly]
    private static nint WndProc(nint hwnd, uint msg, nint wParam, nint lParam)
    {
        var w = _instance;
        if (w is null || hwnd != w._hwnd) return Win32.DefWindowProcW(hwnd, msg, wParam, lParam);
        switch (msg)
        {
            case Win32.WM_HOTKEY:
                if (w._hotkeyNames.TryGetValue((int)wParam, out var name)) Raise(() => w.Hotkey?.Invoke(name));
                return 0;
            case WM_TRAY:
            {
                uint ev = (uint)(lParam & 0xFFFF);
                if (ev == Win32.WM_CONTEXTMENU || ev == Win32.WM_RBUTTONUP) w.ShowMenu();
                else if (ev == Win32.NIN_SELECT || ev == Win32.NIN_KEYSELECT || ev == Win32.WM_LBUTTONUP) Raise(() => w.Command?.Invoke(TrayCommand.Search));
                else if (ev == Win32.NIN_BALLOONUSERCLICK) Raise(() => w.BalloonClicked?.Invoke());
                return 0;
            }
            case WM_INVOKE:
            {
                while (true)
                {
                    Action? a;
                    lock (w._invokes) if (!w._invokes.TryDequeue(out a)) break;
                    Raise(a);
                }
                return 0;
            }
            case Win32.WM_WTSSESSION_CHANGE:
                Raise(() => w.SessionChanged?.Invoke((int)wParam));
                return 0;
            case 0x0010: // WM_CLOSE: quit like the tray menu (DefWindowProc would only destroy the window)
                w.Quit();
                return 0;
            case Win32.WM_DISPLAYCHANGE:
                Raise(() => w.DisplayChanged?.Invoke());
                return 0;
            case Win32.WM_SETTINGCHANGE:   // "ImmersiveColorSet" on a light/dark switch; cheap to check on any change
                w.RefreshTrayIcon();
                break;
        }
        if (msg == TaskbarCreated && TaskbarCreated != 0)
        {
            w._iconAdded = false;
            w.ShowTray(w._tip);
            return 0;
        }
        return Win32.DefWindowProcW(hwnd, msg, wParam, lParam);
    }

    public void Dispose()
    {
        if (_iconAdded)
        {
            var d = NewData();
            Win32.Shell_NotifyIconW(Win32.NIM_DELETE, &d);
            _iconAdded = false;
        }
        if (_trayIcon != 0) { Win32.DestroyIcon(_trayIcon); _trayIcon = 0; }
        foreach (var id in _hotkeyNames.Keys.ToList()) Win32.UnregisterHotKey(_hwnd, id);
        if (_winEventHook != 0) Win32.UnhookWinEvent(_winEventHook);
        Win32.WTSUnRegisterSessionNotification(_hwnd);
        Win32.DestroyWindow(_hwnd);
        _hwnd = 0;
        _instance = null;
    }
}
