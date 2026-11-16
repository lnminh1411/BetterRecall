// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Runtime.InteropServices;

namespace BetterRecall.Agent.Worker;

/// <summary>Windows job object with JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE: every process assigned to it (and its children)
/// is killed when the handle closes, which the OS does when the agent exits or crashes. A hung or orphaned worker then
/// never outlives the agent with its models and device memory held (A2, CA-S1).</summary>
internal sealed unsafe partial class KillOnCloseJob : IDisposable
{
    private nint _handle;

    private KillOnCloseJob(nint handle) => _handle = handle;

    /// <summary>A new job, or null when Windows refuses one (logged; the worker still runs, just without the backstop).</summary>
    public static KillOnCloseJob? TryCreate()
    {
        nint h = CreateJobObjectW(0, 0);
        if (h == 0) return null;
        var info = new JOBOBJECT_EXTENDED_LIMIT_INFORMATION();
        info.Basic.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        if (!SetInformationJobObject(h, JobObjectExtendedLimitInformation, &info, (uint)sizeof(JOBOBJECT_EXTENDED_LIMIT_INFORMATION)))
        {
            CloseHandle(h);
            return null;
        }
        return new KillOnCloseJob(h);
    }

    /// <summary>Puts a running process in the job. False when Windows refuses (the process stays outside it).</summary>
    public bool Assign(nint processHandle) => _handle != 0 && AssignProcessToJobObject(_handle, processHandle);

    /// <summary>Closes the handle: every process in the job is killed.</summary>
    public void Dispose()
    {
        var h = Interlocked.Exchange(ref _handle, 0);
        if (h != 0) CloseHandle(h);
    }

    internal const uint JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE = 0x2000;
    private const int JobObjectExtendedLimitInformation = 9;

    [StructLayout(LayoutKind.Sequential)]
    internal struct JOBOBJECT_BASIC_LIMIT_INFORMATION
    {
        public long PerProcessUserTimeLimit, PerJobUserTimeLimit;
        public uint LimitFlags;
        public nuint MinimumWorkingSetSize, MaximumWorkingSetSize;
        public uint ActiveProcessLimit;
        public nuint Affinity;
        public uint PriorityClass, SchedulingClass;
    }

    [StructLayout(LayoutKind.Sequential)]
    internal struct JOBOBJECT_EXTENDED_LIMIT_INFORMATION
    {
        public JOBOBJECT_BASIC_LIMIT_INFORMATION Basic;
        public ulong ReadOperationCount, WriteOperationCount, OtherOperationCount, ReadTransferCount, WriteTransferCount, OtherTransferCount;
        public nuint ProcessMemoryLimit, JobMemoryLimit, PeakProcessMemoryUsed, PeakJobMemoryUsed;
    }

    [LibraryImport("kernel32.dll", SetLastError = true)]
    private static partial nint CreateJobObjectW(nint attributes, nint name);

    [LibraryImport("kernel32.dll", SetLastError = true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static partial bool SetInformationJobObject(nint job, int infoClass, void* info, uint length);

    [LibraryImport("kernel32.dll", SetLastError = true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static partial bool AssignProcessToJobObject(nint job, nint process);

    [LibraryImport("kernel32.dll", SetLastError = true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static partial bool CloseHandle(nint handle);
}
