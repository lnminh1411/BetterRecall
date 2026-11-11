// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Runtime.InteropServices;
using BetterRecall.Agent.Native;
using BetterRecall.Contracts;

namespace BetterRecall.Agent;

internal enum UserNotificationState { Unknown = 0, NotPresent = 1, Busy = 2, FullscreenD3D = 3, Presentation = 4, AcceptsNotifications = 5, QuietTime = 6, App = 7 }

internal readonly record struct GovernanceInputs(
    double CpuPercent,
    double? IgpuPercent,
    bool OnBattery,
    UserNotificationState Notification,
    long UserIdleMs);

internal readonly record struct GovernanceDecision(bool Run, string Reason, int BatchSize, TimeSpan NextCheck)
{
    public static GovernanceDecision Defer(string reason, TimeSpan next) => new(false, reason, 0, next);
}

/// <summary>Pure scheduling decisions (v1-design section 7), unit-testable.</summary>
internal static class Governance
{
    public const long IdleDrainMs = 60_000;

    public static GovernanceDecision Decide(GovernanceInputs i, GovernanceSettings g, CaptureSettings c)
    {
        if (g.PauseOnBattery && i.OnBattery) return GovernanceDecision.Defer("battery", TimeSpan.FromSeconds(30));
        if (c.PauseOnFullscreen && i.Notification is UserNotificationState.FullscreenD3D or UserNotificationState.Presentation)
            return GovernanceDecision.Defer("fullscreen", TimeSpan.FromSeconds(15));
        if (i.CpuPercent > g.DeferCpuPercent) return GovernanceDecision.Defer("cpu", TimeSpan.FromSeconds(10));
        if (i.IgpuPercent is double gpu && gpu > g.DeferGpuPercent) return GovernanceDecision.Defer("gpu", TimeSpan.FromSeconds(10));
        bool idle = i.UserIdleMs >= IdleDrainMs;
        return new GovernanceDecision(true, idle ? "idle-drain" : "ok", idle ? 32 : 4, TimeSpan.FromSeconds(idle ? 0.5 : 3));
    }

    /// <summary>Keyframes pause in fullscreen D3D and presentation mode when capture.pauseOnFullscreen.</summary>
    public static bool PauseKeyframes(UserNotificationState n, CaptureSettings c) =>
        c.PauseOnFullscreen && n is UserNotificationState.FullscreenD3D or UserNotificationState.Presentation;

    public static unsafe bool ApplyEcoQos(nint process, bool idlePriority)
    {
        var state = new PROCESS_POWER_THROTTLING_STATE
        {
            Version = Win32.PROCESS_POWER_THROTTLING_CURRENT_VERSION,
            ControlMask = Win32.PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
            StateMask = Win32.PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
        };
        bool ok = Win32.SetProcessInformation(process, Win32.ProcessPowerThrottling, &state, (uint)sizeof(PROCESS_POWER_THROTTLING_STATE));
        if (idlePriority) Win32.SetPriorityClass(process, Win32.IDLE_PRIORITY_CLASS);
        return ok;
    }
}

/// <summary>Samples system load for governance. Not thread-safe; one owner.</summary>
internal sealed unsafe class SystemProbe : IDisposable
{
    private ulong _lastIdle, _lastTotal;
    private nint _pdhQuery, _pdhCounter;
    private bool _pdhFailed;
    private readonly string? _igpuLuid;

    public SystemProbe(string? igpuLuidInstanceToken)
    {
        _igpuLuid = igpuLuidInstanceToken;
        CpuPercent();
    }

    public double CpuPercent()
    {
        FILETIME idle, kernel, user;
        if (!Win32.GetSystemTimes(&idle, &kernel, &user)) return 0;
        ulong total = kernel.Value + user.Value;
        ulong dIdle = idle.Value - _lastIdle, dTotal = total - _lastTotal;
        _lastIdle = idle.Value;
        _lastTotal = total;
        if (dTotal == 0) return 0;
        return Math.Clamp(100.0 * (dTotal - dIdle) / dTotal, 0, 100);
    }

    public static bool OnBattery()
    {
        SYSTEM_POWER_STATUS s;
        if (!Win32.GetSystemPowerStatus(&s)) return false;
        return s.ACLineStatus == 0;
    }

    public static UserNotificationState Notification()
    {
        int state;
        return Win32.SHQueryUserNotificationState(&state) == 0 ? (UserNotificationState)state : UserNotificationState.Unknown;
    }

    /// <summary>Highest 3D/compute engine utilisation on the iGPU, from the GPU Engine performance counters. Null when unavailable.</summary>
    public double? IgpuPercent()
    {
        if (_igpuLuid is null || _pdhFailed) return null;
        try
        {
            if (_pdhQuery == 0)
            {
                nint q, c;
                if (Pdh.PdhOpenQueryW(null, 0, &q) != 0) { _pdhFailed = true; return null; }
                if (Pdh.PdhAddEnglishCounterW(q, @"\GPU Engine(*)\Utilization Percentage", 0, &c) != 0) { Pdh.PdhCloseQuery(q); _pdhFailed = true; return null; }
                _pdhQuery = q;
                _pdhCounter = c;
                Pdh.PdhCollectQueryData(q);
                return null; // rate counter needs two samples
            }
            if (Pdh.PdhCollectQueryData(_pdhQuery) != 0) return null;
            uint size = 0, count = 0;
            int rc = Pdh.PdhGetFormattedCounterArrayW(_pdhCounter, Pdh.PDH_FMT_DOUBLE, &size, &count, null);
            if (rc != unchecked((int)Pdh.PDH_MORE_DATA) || size == 0) return null;
            byte* buf = (byte*)NativeMemory.Alloc(size);
            try
            {
                rc = Pdh.PdhGetFormattedCounterArrayW(_pdhCounter, Pdh.PDH_FMT_DOUBLE, &size, &count, (Pdh.PDH_FMT_COUNTERVALUE_ITEM_W*)buf);
                if (rc != 0) return null;
                var items = (Pdh.PDH_FMT_COUNTERVALUE_ITEM_W*)buf;
                var perEngine = new Dictionary<string, double>();
                for (int i = 0; i < count; i++)
                {
                    var name = new string(items[i].szName);
                    if (!name.Contains(_igpuLuid, StringComparison.OrdinalIgnoreCase)) continue;
                    if (!name.EndsWith("engtype_3D", StringComparison.OrdinalIgnoreCase) && !name.EndsWith("engtype_Compute", StringComparison.OrdinalIgnoreCase)) continue;
                    int e = name.IndexOf("_eng_", StringComparison.Ordinal);
                    var key = e >= 0 ? name[e..] : name;
                    perEngine[key] = perEngine.GetValueOrDefault(key) + items[i].FmtValue.doubleValue;
                }
                return perEngine.Count == 0 ? 0 : Math.Min(100, perEngine.Values.Max());
            }
            finally { NativeMemory.Free(buf); }
        }
        catch { _pdhFailed = true; return null; }
    }

    public void Dispose()
    {
        if (_pdhQuery != 0) Pdh.PdhCloseQuery(_pdhQuery);
        _pdhQuery = 0;
    }
}

internal static unsafe partial class Pdh
{
    public const uint PDH_FMT_DOUBLE = 0x00000200;
    public const uint PDH_MORE_DATA = 0x800007D2;

    [StructLayout(LayoutKind.Sequential)]
    public struct PDH_FMT_COUNTERVALUE { public uint CStatus; public double doubleValue; }

    [StructLayout(LayoutKind.Sequential)]
    public struct PDH_FMT_COUNTERVALUE_ITEM_W { public char* szName; public PDH_FMT_COUNTERVALUE FmtValue; }

    [LibraryImport("pdh.dll", StringMarshalling = StringMarshalling.Utf16)]
    public static partial int PdhOpenQueryW(string? source, nint userData, nint* query);

    [LibraryImport("pdh.dll", StringMarshalling = StringMarshalling.Utf16)]
    public static partial int PdhAddEnglishCounterW(nint query, string path, nint userData, nint* counter);

    [LibraryImport("pdh.dll")]
    public static partial int PdhCollectQueryData(nint query);

    [LibraryImport("pdh.dll")]
    public static partial int PdhGetFormattedCounterArrayW(nint counter, uint format, uint* bufferSize, uint* itemCount, PDH_FMT_COUNTERVALUE_ITEM_W* items);

    [LibraryImport("pdh.dll")]
    public static partial int PdhCloseQuery(nint query);
}
