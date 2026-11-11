// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Runtime.InteropServices;

namespace BetterRecall.Agent.Util;

/// <summary>Sub-millisecond sleeps without raising the global timer resolution (CREATE_WAITABLE_TIMER_HIGH_RESOLUTION).</summary>
internal sealed unsafe partial class HighResTimer : IDisposable
{
    private nint _handle;

    public HighResTimer()
    {
        _handle = CreateWaitableTimerExW(0, null, 0x2 /* HIGH_RESOLUTION */, 0x1F0003);
        if (_handle == 0) _handle = CreateWaitableTimerExW(0, null, 0, 0x1F0003);
    }

    /// <summary>Sleeps for the given number of 100 ns units.</summary>
    public void Sleep(long hundredNs)
    {
        if (hundredNs <= 0) return;
        if (_handle == 0) { Thread.Sleep((int)Math.Max(1, hundredNs / 10_000)); return; }
        long due = -hundredNs;
        if (!SetWaitableTimer(_handle, &due, 0, 0, 0, 0)) { Thread.Sleep((int)Math.Max(1, hundredNs / 10_000)); return; }
        WaitForSingleObject(_handle, 0xFFFFFFFF);
    }

    public void Dispose()
    {
        if (_handle != 0) CloseHandle(_handle);
        _handle = 0;
    }

    [LibraryImport("kernel32.dll")]
    private static partial nint CreateWaitableTimerExW(nint attrs, char* name, uint flags, uint access);

    [LibraryImport("kernel32.dll")]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static partial bool SetWaitableTimer(nint timer, long* due, int period, nint completion, nint arg, int resume);

    [LibraryImport("kernel32.dll")]
    private static partial uint WaitForSingleObject(nint handle, uint ms);

    [LibraryImport("kernel32.dll")]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static partial bool CloseHandle(nint handle);
}
