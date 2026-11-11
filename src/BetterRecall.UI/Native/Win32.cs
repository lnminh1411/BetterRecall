// SPDX-License-Identifier: MIT
// Copyright (c) BetterRecall contributors
using System.Runtime.InteropServices;

namespace BetterRecall.UI.Native;

internal static partial class Win32
{
    [LibraryImport("user32.dll")]
    [return: MarshalAs(UnmanagedType.Bool)]
    public static partial bool AllowSetForegroundWindow(uint pid);

    [LibraryImport("user32.dll")]
    [return: MarshalAs(UnmanagedType.Bool)]
    public static partial bool SetForegroundWindow(nint hwnd);

    [LibraryImport("user32.dll")]
    [return: MarshalAs(UnmanagedType.Bool)]
    public static partial bool GetCursorPos(out POINT p);

    [LibraryImport("user32.dll")]
    public static partial uint GetDpiForWindow(nint hwnd);

    [StructLayout(LayoutKind.Sequential)]
    public struct POINT { public int X; public int Y; }
}
