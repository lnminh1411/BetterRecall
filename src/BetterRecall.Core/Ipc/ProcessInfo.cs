// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Diagnostics;
using System.Runtime.InteropServices;
using System.Security.Principal;
using BetterRecall.Contracts;
using BetterRecall.Core.Native;
using Microsoft.Win32.SafeHandles;

namespace BetterRecall.Core.Ipc;

/// <summary>Facts about a peer process read from its token and image name.</summary>
/// <param name="CreatedUtc">Start time of the process inspected, read from the same handle (PID reuse check). Null when unknown.</param>
public sealed record PeerInfo(int Pid, uint Session, string? Sid, int IntegrityRid, bool AppContainer, string? Image, DateTime? CreatedUtc = null);

internal static class ProcessInfo
{
    private static readonly Lazy<uint> OwnSession = new(() =>
        Win32.ProcessIdToSessionId(Win32.GetCurrentProcessId(), out var s) ? s : uint.MaxValue);

    private static readonly Lazy<string> OwnSid = new(() => WindowsIdentity.GetCurrent().User!.Value);

    public static uint CurrentSession => OwnSession.Value;
    public static string CurrentSid => OwnSid.Value;

    public static string? ImagePath(SafeProcessHandle h)
    {
        var buf = new char[32768];
        uint size = (uint)buf.Length;
        return Win32.QueryFullProcessImageName(h, 0, buf, ref size) ? new string(buf, 0, (int)size) : null;
    }

    public static string? ImagePath(int pid)
    {
        using var h = Win32.OpenProcess(Win32.PROCESS_QUERY_LIMITED_INFORMATION, false, (uint)pid);
        return h.IsInvalid ? null : ImagePath(h);
    }

    /// <summary>Process start time (UTC), null on failure.</summary>
    public static DateTime? CreatedUtc(SafeProcessHandle h)
        => Win32.GetProcessTimes(h, out var created, out _, out _, out _) && created > 0 ? DateTime.FromFileTimeUtc(created) : null;

    /// <summary>Start time of the System process (pid 4): the boot, stamped on the clock of that moment. Null when it
    /// cannot be read.</summary>
    private static readonly Lazy<DateTime?> SystemStarted = new(() =>
    {
        using var h = Win32.OpenProcess(Win32.PROCESS_QUERY_LIMITED_INFORMATION, false, 4);
        return h.IsInvalid ? null : CreatedUtc(h);
    });

    /// <summary>Backward clock steps up to this size are left to the wall clock: they reject a new process for at most
    /// this long, and the boot bound below is a few seconds generous (the System process starts after boot).</summary>
    internal static readonly TimeSpan StepTolerance = TimeSpan.FromSeconds(10);

    private static readonly StepClock Steps = new(step => Log.Info(
        $"clock stepped back about {step.TotalSeconds:F0} s: pid-reuse checks accept process start times up to that far ahead until the clock catches up"));

    /// <summary>
    /// Latest start time a process that already ran at wallNowUtc can show (PID reuse checks). Start times are
    /// wall-clock stamps that a later clock change leaves alone: once the clock steps back (NTP fixing a clock that ran
    /// ahead at boot), a process started before the step shows a start "after now", and every peer was taken for a
    /// reused pid until the clock caught up (T2). See StepClock.
    /// </summary>
    public static DateTime StartBound(DateTime wallNowUtc) => Steps.Bound(wallNowUtc, Environment.TickCount64, SystemStarted.Value);

    /// <summary>The rule on a fresh StepClock (one detection), for tests.</summary>
    internal static DateTime StartBound(DateTime wallNowUtc, long msSinceBoot, DateTime? systemStartedUtc)
        => new StepClock().Bound(wallNowUtc, msSinceBoot, systemStartedUtc);


    /// <summary>
    /// Image path of pid, but only when that process started before notAfterUtc (the moment the pipe connected, see StartBound).
    /// A later start means the pid was freed and reused, so the image says nothing about the peer. One handle for both.
    /// </summary>
    public static string? ImagePathIfStartedBefore(int pid, DateTime notAfterUtc)
    {
        using var h = Win32.OpenProcess(Win32.PROCESS_QUERY_LIMITED_INFORMATION, false, (uint)pid);
        if (h.IsInvalid || CreatedUtc(h) is not { } created || created > notAfterUtc) return null;
        return ImagePath(h);
    }

    public static uint SessionOf(int pid) => Win32.ProcessIdToSessionId((uint)pid, out var s) ? s : uint.MaxValue;

    /// <summary>Reads session, user SID, integrity RID, AppContainer flag and image path. Null fields on failure.</summary>
    public static unsafe PeerInfo Read(int pid, uint session)
    {
        string? sid = null, image = null;
        int rid = -1;
        bool appContainer = true;
        using var h = Win32.OpenProcess(Win32.PROCESS_QUERY_LIMITED_INFORMATION, false, (uint)pid);
        if (h.IsInvalid) return new PeerInfo(pid, session, null, -1, true, null);
        image = ImagePath(h);
        var created = CreatedUtc(h);
        if (Win32.OpenProcessToken(h, Win32.TOKEN_QUERY, out var token))
        {
            using (token)
            {
                var buf = Marshal.AllocHGlobal(1024);
                try
                {
                    if (Win32.GetTokenInformation(token, Win32.TokenUser, buf, 1024, out _))
                        sid = new SecurityIdentifier(Marshal.ReadIntPtr(buf)).Value;
                    if (Win32.GetTokenInformation(token, Win32.TokenIntegrityLevel, buf, 1024, out _))
                    {
                        nint psid = Marshal.ReadIntPtr(buf);
                        byte count = *(byte*)Win32.GetSidSubAuthorityCount(psid);
                        if (count > 0) rid = *(int*)Win32.GetSidSubAuthority(psid, (uint)(count - 1));
                    }
                    if (Win32.GetTokenInformation(token, Win32.TokenIsAppContainer, buf, 4, out _))
                        appContainer = Marshal.ReadInt32(buf) != 0;
                }
                finally { Marshal.FreeHGlobal(buf); }
            }
        }
        return new PeerInfo(pid, session, sid, rid, appContainer, image, created);
    }

    public static string Normalize(string path)
    {
        try { return InstallLayout.RealPath(path).TrimEnd('\\'); }
        catch { return path; }
    }

    public static bool SamePath(string? a, string? b)
        => a != null && b != null && string.Equals(Normalize(a), Normalize(b), StringComparison.OrdinalIgnoreCase);

    public static int CurrentPid => Environment.ProcessId;

    public static string? CurrentImage => Environment.ProcessPath ?? Process.GetCurrentProcess().MainModule?.FileName;
}

/// <summary>
/// T2, S6: detects a backward clock step and bounds start times for its catch-up period only. Boot start (System
/// process, pid 4) plus the time since boot (TickCount64 counts sleep and does not step) runs ahead of the wall clock by
/// the net size of all steps back since boot. When that gap grows by more than StepTolerance, a step back of that size
/// just happened: every process started before it shows a start no later than wall + step, so that is the bound until
/// the wall clock reaches it; afterwards the wall clock alone is the bound again. Small gaps (the System process starts
/// a few seconds after boot, tick drift) never count.
/// </summary>
internal sealed class StepClock(Action<TimeSpan>? onStep = null)
{
    private readonly object _gate = new();
    private TimeSpan _gap;       // boot clock minus wall clock as last accounted for
    private DateTime? _until;    // end of the current catch-up period

    public DateTime Bound(DateTime wallNowUtc, long msSinceBoot, DateTime? systemStartedUtc)
    {
        if (systemStartedUtc is not { } boot) return wallNowUtc;
        var gap = boot.AddMilliseconds(msSinceBoot) - wallNowUtc;
        TimeSpan? step = null;
        DateTime bound;
        lock (_gate)
        {
            if (gap > _gap + ProcessInfo.StepTolerance)
            {
                step = gap - _gap;
                var next = wallNowUtc + step.Value;
                _until = _until is { } u && u > next ? u : next;
                _gap = gap;
            }
            else if (gap < _gap)
            {
                _gap = gap;   // follow forward steps and wall ahead of boot; never creep up across sub-tolerance steps back
            }
            bound = _until is { } until && wallNowUtc < until ? until : wallNowUtc;
        }
        if (step is { } s) onStep?.Invoke(s);
        return bound;
    }
}
