// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Diagnostics;
using System.Text;
using BetterRecall.Agent.Security;
using BetterRecall.Agent.Util;

namespace BetterRecall.Agent.Host;

/// <summary>Measurement dump for dev runs (--exit-after N). Counts and timings only, never content.</summary>
internal static class Diagnostics
{
    private static TimeSpan _cpuMark;
    private static long _wallMark;

    /// <summary>Starts a CPU window (call after warm-up).</summary>
    public static void Mark()
    {
        using var p = Process.GetCurrentProcess();
        _cpuMark = p.TotalProcessorTime;
        _wallMark = Stopwatch.GetTimestamp();
    }

    public static string Report(AgentHost host)
    {
        var sb = new StringBuilder();
        using var p = Process.GetCurrentProcess();
        p.Refresh();
        if (_wallMark != 0)
        {
            var cpu = p.TotalProcessorTime - _cpuMark;
            var wall = Stopwatch.GetElapsedTime(_wallMark);
            sb.AppendLine($"cpu {cpu.TotalMilliseconds / wall.TotalMilliseconds * 100:F2}% of one core over {wall.TotalSeconds:F0} s");
        }
        sb.AppendLine($"working set {p.WorkingSet64 / 1048576} MB, private {p.PrivateMemorySize64 / 1048576} MB, threads {p.Threads.Count}");
        var cap = host.Capture;
        if (cap is not null)
        {
            sb.AppendLine($"replay ring committed {cap.ReplayRingBytes() / 1048576.0:F1} MiB, guard skips {cap.SkippedByGuard} (last {cap.LastGuardReason})");
            foreach (var o in cap.Outputs)
            {
                double grab = o.GrabCount == 0 ? 0 : o.GrabTicks * 1000.0 / Stopwatch.Frequency / o.GrabCount;
                sb.AppendLine($"output {o.Info.Index} {o.Info.Width}x{o.Info.Height}: {o.State}, frames {o.FrameCount}, grabs {o.GrabCount} ({grab:F2} ms), no-change skips {o.SkippedNoChange}");
            }
            foreach (var rp in cap.Engine.Pipelines)
            {
                double copyMs = rp.CopyCount == 0 ? 0 : rp.CopyTicks * 1000.0 / Stopwatch.Frequency / rp.CopyCount;
                sb.AppendLine($"replay {rp.Spec.Key} {rp.Encoder.Codec} {rp.Encoder.Api} '{rp.Encoder.Name}' {rp.Encoder.Width}x{rp.Encoder.Height}@{rp.Spec.Fps}: in {rp.Encoder.FramesIn} out {rp.Encoder.PacketsOut} dropped {rp.Encoder.FramesDropped} busy {rp.BusyCount} forcedKeys {rp.Encoder.ForcedKeyframes} repeats {rp.RepeatCount} copy {copyMs:F2} ms p95 {rp.Encoder.Latency.Percentile(95):F2} ms, ring {rp.Stream.Ring.CommittedBytes / 1048576.0:F1}/{rp.Stream.Ring.Capacity / 1048576.0:F0} MiB, lockFailed {rp.Stream.Ring.LockFailed}");
            }
            if (!string.IsNullOrEmpty(cap.Engine.LastError))
                sb.AppendLine($"replay error: {cap.Engine.LastError}");
            if (cap.Audio is { } a) sb.AppendLine($"replay audio: packets {a.Ring.Count}, ring {a.Ring.CommittedBytes / 1048576.0:F1} MiB");
        }
        if (host.Pipeline is { } kp)
        {
            double enc = kp.Kept == 0 ? 0 : kp.EncodeMsTotal / kp.Kept;
            sb.AppendLine($"keyframes kept {kp.Kept}, similar {kp.SkippedSimilar}, dropped {kp.Dropped}, failed {kp.Failed}, encode avg {enc:F1} ms");
        }
        if (host.Scheduler is { } sc) sb.AppendLine($"scheduler: {sc.LastDecision}, processed {sc.Processed}, nsfw {sc.Dropped}, queued {sc.Queued}");
        if (host.Guard is { } g) sb.AppendLine($"guard scan {g.LastScanMs:F2} ms, badge {g.LastBadgeMs:F1} ms");
        sb.AppendLine($"worker running {host.Supervisor.IsRunning}, failures {host.Supervisor.Failures}, nsfw device {host.Supervisor.Hello?.NsfwDevice ?? "not loaded"}");
        sb.AppendLine($"isolation applied: {string.Join(", ", MemoryIsolation.Applied)}; skipped: {string.Join(", ", MemoryIsolation.Skipped)}");
        return sb.ToString();
    }

    public static void DumpAndQuit(AgentHost host)
    {
        try
        {
            var report = Report(host);
            Log.Info("diagnostics\n" + report);
            var path = Environment.GetEnvironmentVariable("BETTERRECALL_DIAG_FILE");
            if (!string.IsNullOrEmpty(path)) File.WriteAllText(path, report);
        }
        catch (Exception ex) { Log.Warn("diagnostics", ex); }
        host.RequestQuit();
    }
}
