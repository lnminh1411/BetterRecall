// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Runtime.InteropServices;
using System.Text;
using BetterRecall.Contracts;
using Microsoft.Win32.SafeHandles;

namespace BetterRecall.UI.Logic;

public sealed record TrustCheck(bool Trusted, int ServerPid);

/// <summary>Client side of research.md 7.4: the server process must be the installed agent in this session.</summary>
public interface IServerTrust
{
    TrustCheck Verify(SafePipeHandle pipe);
}

public sealed class ServerTrustVerifier : IServerTrust
{
    private readonly string _expectedImage;
    private readonly Func<SafePipeHandle, int?> _serverPid;
    private readonly Func<int, DateTime, string?> _imageOf;
    private readonly Func<int, int?> _sessionOf;
    private readonly int _ownSession;
    private readonly Func<DateTime> _startBound;

    public ServerTrustVerifier(
        string expectedImage,
        Func<SafePipeHandle, int?>? serverPid = null,
        Func<int, string?>? imageOf = null,
        Func<int, int?>? sessionOf = null,
        int? ownSession = null,
        Func<int, DateTime?>? startedOf = null,
        Func<DateTime>? startBound = null)
    {
        _expectedImage = Normalize(expectedImage);
        _serverPid = serverPid ?? Win32Trust.ServerPid;
        // Default: image and start time from one process handle. A process that started after the connection is not the
        // server that accepted it: its pid was freed and reused.
        Func<int, DateTime, string?> image = imageOf is null ? Win32Trust.ImagePathIfStartedBefore : (pid, _) => imageOf(pid);
        _imageOf = startedOf is null ? image : (pid, at) => startedOf(pid) is { } s && s <= at ? image(pid, at) : null;
        _sessionOf = sessionOf ?? Win32Trust.SessionOf;
        _ownSession = ownSession ?? Win32Trust.SessionOf(Environment.ProcessId) ?? -1;
        _startBound = startBound ?? (() => Steps.Bound(DateTime.UtcNow, Environment.TickCount64, Win32Trust.SystemStartedUtc.Value));
    }

    /// <summary>Backward clock steps up to this size are left to the wall clock (same rule as Core ProcessInfo).</summary>
    public static readonly TimeSpan StepTolerance = TimeSpan.FromSeconds(10);

    private static readonly StepClock Steps = new();

    /// <summary>Latest start time a process that already ran at wallNowUtc can show, on a fresh StepClock (one
    /// detection). Same rule as Core ProcessInfo.StartBound; see StepClock.</summary>
    public static DateTime StartBound(DateTime wallNowUtc, long msSinceBoot, DateTime? systemStartedUtc)
        => new StepClock().Bound(wallNowUtc, msSinceBoot, systemStartedUtc);

    /// <summary>The agent path for the installed layout: install dir is the parent of the UI exe's folder.</summary>
    public static string InstalledAgentPath(string uiBaseDirectory)
    {
        var uiDir = Path.GetFullPath(uiBaseDirectory).TrimEnd(Path.DirectorySeparatorChar, Path.AltDirectorySeparatorChar);
        var install = Path.GetDirectoryName(uiDir) ?? uiDir;
        return InstallLayout.Full(install, InstallLayout.AgentExe);
    }

    public TrustCheck Verify(SafePipeHandle pipe)
    {
        var connectedUtc = _startBound();   // Verify runs right after the connect
        var pid = _serverPid(pipe);
        if (pid is null or <= 0) return new TrustCheck(false, 0);
        var image = _imageOf(pid.Value, connectedUtc);
        if (image is null || !string.Equals(Normalize(image), _expectedImage, StringComparison.OrdinalIgnoreCase))
            return new TrustCheck(false, pid.Value);
        var session = _sessionOf(pid.Value);
        if (session is null || session.Value != _ownSession) return new TrustCheck(false, pid.Value);
        return new TrustCheck(true, pid.Value);
    }

    private static string Normalize(string path)
    {
        try { return InstallLayout.RealPath(path); } catch { return path; }
    }
}

internal static partial class Win32Trust
{
    private const uint ProcessQueryLimitedInformation = 0x1000;

    [LibraryImport("kernel32.dll", SetLastError = true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static partial bool GetNamedPipeServerProcessId(SafePipeHandle pipe, out uint pid);

    [LibraryImport("kernel32.dll", SetLastError = true)]
    private static partial nint OpenProcess(uint access, [MarshalAs(UnmanagedType.Bool)] bool inherit, uint pid);

    [LibraryImport("kernel32.dll")]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static partial bool CloseHandle(nint h);

    [LibraryImport("kernel32.dll", EntryPoint = "QueryFullProcessImageNameW", SetLastError = true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static unsafe partial bool QueryFullProcessImageName(nint process, uint flags, char* buffer, ref uint size);

    [LibraryImport("kernel32.dll", SetLastError = true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static partial bool ProcessIdToSessionId(uint pid, out uint session);

    public static int? ServerPid(SafePipeHandle pipe) => GetNamedPipeServerProcessId(pipe, out var pid) ? (int)pid : null;

    public static unsafe string? ImagePath(int pid)
    {
        var h = OpenProcess(ProcessQueryLimitedInformation, false, (uint)pid);
        if (h == 0) return null;
        try
        {
            var buf = new char[32768];
            uint size = (uint)buf.Length;
            fixed (char* p = buf)
                return QueryFullProcessImageName(h, 0, p, ref size) ? new string(buf, 0, (int)size) : null;
        }
        finally { CloseHandle(h); }
    }

    [LibraryImport("kernel32.dll", SetLastError = true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static partial bool GetProcessTimes(nint process, out long creation, out long exit, out long kernel, out long user);

    /// <summary>Image path of pid when that process started no later than notAfterUtc, else null (PID reuse). One handle.</summary>
    public static unsafe string? ImagePathIfStartedBefore(int pid, DateTime notAfterUtc)
    {
        var h = OpenProcess(ProcessQueryLimitedInformation, false, (uint)pid);
        if (h == 0) return null;
        try
        {
            if (!GetProcessTimes(h, out var created, out _, out _, out _) || created <= 0 || DateTime.FromFileTimeUtc(created) > notAfterUtc) return null;
            var buf = new char[32768];
            uint size = (uint)buf.Length;
            fixed (char* p = buf)
                return QueryFullProcessImageName(h, 0, p, ref size) ? new string(buf, 0, (int)size) : null;
        }
        finally { CloseHandle(h); }
    }

    public static int? SessionOf(int pid) => ProcessIdToSessionId((uint)pid, out var s) ? (int)s : null;

    /// <summary>Start time of the System process (pid 4), i.e. the boot on the clock of that moment; null when unreadable.</summary>
    public static readonly Lazy<DateTime?> SystemStartedUtc = new(() =>
    {
        var h = OpenProcess(ProcessQueryLimitedInformation, false, 4);
        if (h == 0) return null;
        try { return GetProcessTimes(h, out var created, out _, out _, out _) && created > 0 ? DateTime.FromFileTimeUtc(created) : null; }
        finally { CloseHandle(h); }
    });
}

/// <summary>
/// T2, S6 (same rule as Core StepClock): start times are wall-clock stamps a later clock change leaves alone, so after a
/// step back the running agent "started in the future" and was refused until the clock caught up. Boot start (System
/// process) plus the time since boot (TickCount64, does not step) runs ahead of the wall clock by the net size of the
/// steps back since boot. When that gap grows by more than StepTolerance, a step back of that size just happened: the
/// bound is wall + step until the wall clock reaches it, then the wall clock alone again.
/// </summary>
public sealed class StepClock(Action<TimeSpan>? onStep = null)
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
            if (gap > _gap + ServerTrustVerifier.StepTolerance)
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
