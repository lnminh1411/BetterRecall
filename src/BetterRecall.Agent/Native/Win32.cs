// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Runtime.InteropServices;

namespace BetterRecall.Agent.Native;

// Hand-written LibraryImport declarations: blittable, no runtime marshalling, Native AOT safe.

[StructLayout(LayoutKind.Sequential)]
internal struct RECT
{
    public int Left, Top, Right, Bottom;
    public readonly int Width => Right - Left;
    public readonly int Height => Bottom - Top;
    public RECT(int l, int t, int r, int b) { Left = l; Top = t; Right = r; Bottom = b; }
}

[StructLayout(LayoutKind.Sequential)]
internal struct POINT { public int X, Y; }

[StructLayout(LayoutKind.Sequential)]
internal struct MSG
{
    public nint hwnd;
    public uint message;
    public nint wParam;
    public nint lParam;
    public uint time;
    public POINT pt;
    public uint lPrivate;
}

[StructLayout(LayoutKind.Sequential)]
internal unsafe struct WNDCLASSEXW
{
    public uint cbSize;
    public uint style;
    public delegate* unmanaged<nint, uint, nint, nint, nint> lpfnWndProc;
    public int cbClsExtra;
    public int cbWndExtra;
    public nint hInstance;
    public nint hIcon;
    public nint hCursor;
    public nint hbrBackground;
    public char* lpszMenuName;
    public char* lpszClassName;
    public nint hIconSm;
}

[StructLayout(LayoutKind.Sequential)]
internal unsafe struct NOTIFYICONDATAW
{
    public uint cbSize;
    public nint hWnd;
    public uint uID;
    public uint uFlags;
    public uint uCallbackMessage;
    public nint hIcon;
    public fixed char szTip[128];
    public uint dwState;
    public uint dwStateMask;
    public fixed char szInfo[256];
    public uint uVersion;          // union with uTimeout
    public fixed char szInfoTitle[64];
    public uint dwInfoFlags;
    public Guid guidItem;
    public nint hBalloonIcon;
}

[StructLayout(LayoutKind.Sequential)]
internal struct LASTINPUTINFO { public uint cbSize; public uint dwTime; }

[StructLayout(LayoutKind.Sequential)]
internal struct SYSTEM_POWER_STATUS
{
    public byte ACLineStatus;
    public byte BatteryFlag;
    public byte BatteryLifePercent;
    public byte SystemStatusFlag;
    public uint BatteryLifeTime;
    public uint BatteryFullLifeTime;
}

[StructLayout(LayoutKind.Sequential)]
internal struct FILETIME { public uint Low, High; public readonly ulong Value => ((ulong)High << 32) | Low; }

[StructLayout(LayoutKind.Sequential)]
internal struct PROCESS_POWER_THROTTLING_STATE { public uint Version, ControlMask, StateMask; }

[StructLayout(LayoutKind.Sequential)]
internal struct MONITORINFO { public uint cbSize; public RECT rcMonitor; public RECT rcWork; public uint dwFlags; }

internal static unsafe partial class Win32
{
    public const uint WM_DESTROY = 0x0002;
    public const uint WM_CLOSE = 0x0010;
    public const uint WM_QUIT = 0x0012;
    public const uint WM_COMMAND = 0x0111;
    public const uint WM_HOTKEY = 0x0312;
    public const uint WM_USER = 0x0400;
    public const uint WM_APP = 0x8000;
    public const uint WM_WTSSESSION_CHANGE = 0x02B1;
    public const uint WM_POWERBROADCAST = 0x0218;
    public const uint WM_LBUTTONUP = 0x0202;
    public const uint WM_RBUTTONUP = 0x0205;
    public const uint WM_CONTEXTMENU = 0x007B;
    public const uint WM_DISPLAYCHANGE = 0x007E;

    public const int WTS_CONSOLE_CONNECT = 1, WTS_CONSOLE_DISCONNECT = 2, WTS_REMOTE_CONNECT = 3, WTS_REMOTE_DISCONNECT = 4,
        WTS_SESSION_LOGON = 5, WTS_SESSION_LOGOFF = 6, WTS_SESSION_LOCK = 7, WTS_SESSION_UNLOCK = 8;

    public const uint MOD_ALT = 0x1, MOD_CONTROL = 0x2, MOD_SHIFT = 0x4, MOD_WIN = 0x8, MOD_NOREPEAT = 0x4000;

    public const uint NIM_ADD = 0, NIM_MODIFY = 1, NIM_DELETE = 2, NIM_SETVERSION = 4;
    public const uint NIF_MESSAGE = 1, NIF_ICON = 2, NIF_TIP = 4, NIF_INFO = 0x10, NIF_SHOWTIP = 0x80;
    public const uint NIIF_INFO = 1, NIIF_NOSOUND = 0x10;
    public const uint NOTIFYICON_VERSION_4 = 4;
    public const uint NIN_SELECT = WM_USER + 0, NIN_KEYSELECT = WM_USER + 1, NIN_BALLOONUSERCLICK = WM_USER + 5;

    public const uint MF_STRING = 0, MF_SEPARATOR = 0x800, MF_POPUP = 0x10, MF_GRAYED = 0x1, MF_CHECKED = 0x8;
    public const uint TPM_RIGHTBUTTON = 0x2, TPM_RETURNCMD = 0x100, TPM_NONOTIFY = 0x80;

    public const uint EVENT_SYSTEM_FOREGROUND = 0x0003;
    public const uint WINEVENT_OUTOFCONTEXT = 0, WINEVENT_SKIPOWNPROCESS = 2;

    public const uint WDA_NONE = 0;
    public const uint GA_ROOT = 2;
    public const int GWL_EXSTYLE = -20;
    public const uint WS_EX_TOOLWINDOW = 0x80;
    public const uint DWMWA_CLOAKED = 14;
    public const uint DWMWA_EXTENDED_FRAME_BOUNDS = 9;

    public const uint PROCESS_QUERY_LIMITED_INFORMATION = 0x1000;
    public const uint PROCESS_SET_INFORMATION = 0x0200;

    // ---- user32 ----
    [LibraryImport("user32.dll", EntryPoint = "RegisterClassExW", SetLastError = true)]
    public static partial ushort RegisterClassExW(WNDCLASSEXW* cls);

    [LibraryImport("user32.dll", EntryPoint = "CreateWindowExW", SetLastError = true)]
    public static partial nint CreateWindowExW(uint exStyle, char* className, char* windowName, uint style, int x, int y, int w, int h, nint parent, nint menu, nint instance, nint param);

    [LibraryImport("user32.dll")]
    public static partial nint DefWindowProcW(nint hwnd, uint msg, nint wParam, nint lParam);

    [LibraryImport("user32.dll")]
    [return: MarshalAs(UnmanagedType.Bool)]
    public static partial bool DestroyWindow(nint hwnd);

    [LibraryImport("user32.dll")]
    public static partial int GetMessageW(MSG* msg, nint hwnd, uint min, uint max);

    [LibraryImport("user32.dll")]
    [return: MarshalAs(UnmanagedType.Bool)]
    public static partial bool TranslateMessage(MSG* msg);

    [LibraryImport("user32.dll")]
    public static partial nint DispatchMessageW(MSG* msg);

    [LibraryImport("user32.dll")]
    [return: MarshalAs(UnmanagedType.Bool)]
    public static partial bool PostMessageW(nint hwnd, uint msg, nint wParam, nint lParam);

    [LibraryImport("user32.dll")]
    public static partial void PostQuitMessage(int code);

    [LibraryImport("user32.dll", EntryPoint = "RegisterWindowMessageW", StringMarshalling = StringMarshalling.Utf16)]
    public static partial uint RegisterWindowMessageW(string name);

    [LibraryImport("user32.dll", SetLastError = true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    public static partial bool RegisterHotKey(nint hwnd, int id, uint modifiers, uint vk);

    [LibraryImport("user32.dll")]
    [return: MarshalAs(UnmanagedType.Bool)]
    public static partial bool UnregisterHotKey(nint hwnd, int id);

    [LibraryImport("user32.dll")]
    public static partial nint CreatePopupMenu();

    [LibraryImport("user32.dll", EntryPoint = "AppendMenuW", StringMarshalling = StringMarshalling.Utf16)]
    [return: MarshalAs(UnmanagedType.Bool)]
    public static partial bool AppendMenuW(nint menu, uint flags, nuint id, string? text);

    [LibraryImport("user32.dll")]
    public static partial int TrackPopupMenuEx(nint menu, uint flags, int x, int y, nint hwnd, nint tpm);

    [LibraryImport("user32.dll")]
    [return: MarshalAs(UnmanagedType.Bool)]
    public static partial bool DestroyMenu(nint menu);

    [LibraryImport("user32.dll")]
    [return: MarshalAs(UnmanagedType.Bool)]
    public static partial bool GetCursorPos(POINT* pt);

    [LibraryImport("user32.dll")]
    [return: MarshalAs(UnmanagedType.Bool)]
    public static partial bool SetForegroundWindow(nint hwnd);

    [LibraryImport("user32.dll")]
    [return: MarshalAs(UnmanagedType.Bool)]
    public static partial bool AllowSetForegroundWindow(uint pid);

    [LibraryImport("user32.dll")]
    public static partial nint LoadIconW(nint instance, nint name);

    public const uint IMAGE_ICON = 1, LR_LOADFROMFILE = 0x10;
    public const int SM_CXSMICON = 49;
    public const uint WM_SETTINGCHANGE = 0x001A;

    [LibraryImport("user32.dll", StringMarshalling = StringMarshalling.Utf16)]
    public static partial nint LoadImageW(nint instance, string name, uint type, int cx, int cy, uint flags);

    [LibraryImport("user32.dll")]
    [return: MarshalAs(UnmanagedType.Bool)]
    public static partial bool DestroyIcon(nint icon);

    [LibraryImport("user32.dll")]
    public static partial int GetSystemMetrics(int index);

    [LibraryImport("user32.dll")]
    public static partial nint GetForegroundWindow();

    [LibraryImport("user32.dll")]
    public static partial uint GetWindowThreadProcessId(nint hwnd, uint* pid);

    [LibraryImport("user32.dll", EntryPoint = "GetWindowTextW")]
    public static partial int GetWindowTextW(nint hwnd, char* buffer, int max);

    [LibraryImport("user32.dll", EntryPoint = "GetWindowTextLengthW")]
    public static partial int GetWindowTextLengthW(nint hwnd);

    [LibraryImport("user32.dll", EntryPoint = "GetClassNameW")]
    public static partial int GetClassNameW(nint hwnd, char* buffer, int max);

    [LibraryImport("user32.dll")]
    [return: MarshalAs(UnmanagedType.Bool)]
    public static partial bool IsWindowVisible(nint hwnd);

    [LibraryImport("user32.dll")]
    [return: MarshalAs(UnmanagedType.Bool)]
    public static partial bool IsIconic(nint hwnd);

    [LibraryImport("user32.dll")]
    [return: MarshalAs(UnmanagedType.Bool)]
    public static partial bool GetWindowRect(nint hwnd, RECT* rect);

    [LibraryImport("user32.dll")]
    [return: MarshalAs(UnmanagedType.Bool)]
    public static partial bool GetWindowDisplayAffinity(nint hwnd, uint* affinity);

    [LibraryImport("user32.dll")]
    [return: MarshalAs(UnmanagedType.Bool)]
    public static partial bool EnumWindows(delegate* unmanaged<nint, nint, int> callback, nint lParam);

    [LibraryImport("user32.dll")]
    public static partial nint GetAncestor(nint hwnd, uint flags);

    [LibraryImport("user32.dll", EntryPoint = "GetWindowLongPtrW")]
    public static partial nint GetWindowLongPtrW(nint hwnd, int index);

    [LibraryImport("user32.dll")]
    [return: MarshalAs(UnmanagedType.Bool)]
    public static partial bool GetLastInputInfo(LASTINPUTINFO* info);

    [LibraryImport("user32.dll")]
    public static partial nint SetWinEventHook(uint min, uint max, nint module, delegate* unmanaged<nint, uint, nint, int, int, uint, uint, void> proc, uint pid, uint tid, uint flags);

    [LibraryImport("user32.dll")]
    [return: MarshalAs(UnmanagedType.Bool)]
    public static partial bool UnhookWinEvent(nint hook);

    [LibraryImport("user32.dll")]
    public static partial nint MonitorFromWindow(nint hwnd, uint flags);

    [LibraryImport("user32.dll", EntryPoint = "GetMonitorInfoW")]
    [return: MarshalAs(UnmanagedType.Bool)]
    public static partial bool GetMonitorInfoW(nint monitor, MONITORINFO* info);

    [LibraryImport("user32.dll")]
    public static partial uint GetDpiForWindow(nint hwnd);

    // ---- shell32 ----
    [LibraryImport("shell32.dll", EntryPoint = "Shell_NotifyIconW")]
    [return: MarshalAs(UnmanagedType.Bool)]
    public static partial bool Shell_NotifyIconW(uint message, NOTIFYICONDATAW* data);

    [LibraryImport("shell32.dll")]
    public static partial int SHQueryUserNotificationState(int* state);

    public const int QUNS_NOT_PRESENT = 1, QUNS_BUSY = 2, QUNS_RUNNING_D3D_FULL_SCREEN = 3, QUNS_PRESENTATION_MODE = 4,
        QUNS_ACCEPTS_NOTIFICATIONS = 5, QUNS_QUIET_TIME = 6, QUNS_APP = 7;

    // ---- wtsapi32 ----
    [LibraryImport("wtsapi32.dll")]
    [return: MarshalAs(UnmanagedType.Bool)]
    public static partial bool WTSRegisterSessionNotification(nint hwnd, uint flags);

    [LibraryImport("wtsapi32.dll")]
    [return: MarshalAs(UnmanagedType.Bool)]
    public static partial bool WTSUnRegisterSessionNotification(nint hwnd);

    // ---- kernel32 ----
    [LibraryImport("kernel32.dll")]
    public static partial nint GetModuleHandleW(char* name);

    [LibraryImport("kernel32.dll", SetLastError = true)]
    public static partial nint OpenProcess(uint access, [MarshalAs(UnmanagedType.Bool)] bool inherit, uint pid);

    [LibraryImport("kernel32.dll")]
    [return: MarshalAs(UnmanagedType.Bool)]
    public static partial bool CloseHandle(nint handle);

    [LibraryImport("kernel32.dll", EntryPoint = "QueryFullProcessImageNameW")]
    [return: MarshalAs(UnmanagedType.Bool)]
    public static partial bool QueryFullProcessImageNameW(nint process, uint flags, char* buffer, uint* size);

    [LibraryImport("kernel32.dll")]
    [return: MarshalAs(UnmanagedType.Bool)]
    public static partial bool GetSystemTimes(FILETIME* idle, FILETIME* kernel, FILETIME* user);

    [LibraryImport("kernel32.dll")]
    [return: MarshalAs(UnmanagedType.Bool)]
    public static partial bool GetProcessTimes(nint process, FILETIME* creation, FILETIME* exit, FILETIME* kernel, FILETIME* user);

    [LibraryImport("kernel32.dll")]
    [return: MarshalAs(UnmanagedType.Bool)]
    public static partial bool GetSystemPowerStatus(SYSTEM_POWER_STATUS* status);

    [LibraryImport("kernel32.dll")]
    public static partial nint GetCurrentProcess();

    [LibraryImport("kernel32.dll")]
    public static partial uint GetCurrentProcessId();

    [LibraryImport("kernel32.dll")]
    public static partial uint GetTickCount();

    [LibraryImport("kernel32.dll")]
    public static partial ulong GetTickCount64();

    [LibraryImport("kernel32.dll", SetLastError = true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    public static partial bool SetProcessInformation(nint process, int infoClass, void* info, uint size);

    public const int ProcessPowerThrottling = 4;
    public const uint PROCESS_POWER_THROTTLING_CURRENT_VERSION = 1;
    public const uint PROCESS_POWER_THROTTLING_EXECUTION_SPEED = 1;

    [LibraryImport("kernel32.dll", SetLastError = true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    public static partial bool SetPriorityClass(nint process, uint priorityClass);

    public const uint IDLE_PRIORITY_CLASS = 0x40, BELOW_NORMAL_PRIORITY_CLASS = 0x4000;

    [LibraryImport("kernel32.dll", SetLastError = true)]
    public static partial void* VirtualAlloc(void* address, nuint size, uint allocationType, uint protect);

    [LibraryImport("kernel32.dll", SetLastError = true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    public static partial bool VirtualFree(void* address, nuint size, uint freeType);

    [LibraryImport("kernel32.dll", SetLastError = true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    public static partial bool VirtualLock(void* address, nuint size);

    [LibraryImport("kernel32.dll", SetLastError = true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    public static partial bool VirtualUnlock(void* address, nuint size);

    public const uint MEM_COMMIT = 0x1000, MEM_RESERVE = 0x2000, MEM_RELEASE = 0x8000, PAGE_READWRITE = 0x04;

    [LibraryImport("kernel32.dll", SetLastError = true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    public static partial bool GetProcessWorkingSetSizeEx(nint process, nuint* min, nuint* max, uint* flags);

    [LibraryImport("kernel32.dll", SetLastError = true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    public static partial bool SetProcessWorkingSetSizeEx(nint process, nuint min, nuint max, uint flags);

    public const uint QUOTA_LIMITS_HARDWS_MIN_ENABLE = 0x1, QUOTA_LIMITS_HARDWS_MAX_DISABLE = 0x8;

    [LibraryImport("kernel32.dll", SetLastError = true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    public static partial bool SetProcessMitigationPolicy(int policy, void* buffer, nuint length);

    public const int ProcessDynamicCodePolicy = 2, ProcessStrictHandleCheckPolicy = 3, ProcessExtensionPointDisablePolicy = 6,
        ProcessImageLoadPolicy = 10, ProcessChildProcessPolicy = 13;

    [LibraryImport("kernel32.dll", SetLastError = true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    public static partial bool GetNamedPipeClientProcessId(nint pipe, uint* pid);

    [LibraryImport("kernel32.dll", SetLastError = true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    public static partial bool QueryPerformanceCounter(long* value);

    [LibraryImport("kernel32.dll")]
    public static partial uint WTSGetActiveConsoleSessionId();

    [LibraryImport("kernel32.dll")]
    [return: MarshalAs(UnmanagedType.Bool)]
    public static partial bool ProcessIdToSessionId(uint pid, uint* session);

    // ---- advapi32 ----
    [LibraryImport("advapi32.dll", EntryPoint = "ConvertStringSecurityDescriptorToSecurityDescriptorW", StringMarshalling = StringMarshalling.Utf16, SetLastError = true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    public static partial bool ConvertStringSecurityDescriptorToSecurityDescriptorW(string sddl, uint revision, nint* sd, uint* size);

    [LibraryImport("advapi32.dll", SetLastError = true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    public static partial bool SetKernelObjectSecurity(nint handle, uint info, nint sd);

    public const uint DACL_SECURITY_INFORMATION = 0x4;

    [LibraryImport("kernel32.dll")]
    public static partial nint LocalFree(nint mem);

    // ---- wer ----
    [LibraryImport("wer.dll", StringMarshalling = StringMarshalling.Utf16)]
    public static partial int WerAddExcludedApplication(string exeName, [MarshalAs(UnmanagedType.Bool)] bool allUsers);

    // ---- dwmapi ----
    [LibraryImport("dwmapi.dll")]
    public static partial int DwmGetWindowAttribute(nint hwnd, uint attr, void* value, uint size);

    // ---- helpers ----
    public static string GetWindowText(nint hwnd)
    {
        int len = GetWindowTextLengthW(hwnd);
        if (len <= 0) return "";
        len = Math.Min(len, 1024);
        char* buf = stackalloc char[len + 1];
        int n = GetWindowTextW(hwnd, buf, len + 1);
        return n <= 0 ? "" : new string(buf, 0, n);
    }

    public static string GetClassName(nint hwnd)
    {
        char* buf = stackalloc char[256];
        int n = GetClassNameW(hwnd, buf, 256);
        return n <= 0 ? "" : new string(buf, 0, n);
    }

    public static string? GetProcessImagePath(uint pid)
    {
        nint h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid);
        if (h == 0) return null;
        try
        {
            char* buf = stackalloc char[1024];
            uint size = 1024;
            return QueryFullProcessImageNameW(h, 0, buf, &size) ? new string(buf, 0, (int)size) : null;
        }
        finally { CloseHandle(h); }
    }

    public static uint IdleMilliseconds()
    {
        var info = new LASTINPUTINFO { cbSize = (uint)sizeof(LASTINPUTINFO) };
        if (!GetLastInputInfo(&info)) return 0;
        return unchecked(GetTickCount() - info.dwTime);
    }

    public static long Qpc() { long v; QueryPerformanceCounter(&v); return v; }

    public static bool IsCloaked(nint hwnd)
    {
        int cloaked = 0;
        return DwmGetWindowAttribute(hwnd, DWMWA_CLOAKED, &cloaked, 4) == 0 && cloaked != 0;
    }
}
