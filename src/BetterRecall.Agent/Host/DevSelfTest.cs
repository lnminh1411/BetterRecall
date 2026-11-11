// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Diagnostics;
using BetterRecall.Agent.Audio;
using BetterRecall.Agent.Capture;
using BetterRecall.Agent.Native;
using BetterRecall.Agent.Replay;
using BetterRecall.Agent.Util;
using BetterRecall.Contracts;

namespace BetterRecall.Agent.Host;

/// <summary>
/// Developer self-tests that need no Core and no data dir: "--selftest-replay seconds out.mp4 [fps] [kbps] [encoder]"
/// records every output into the replay rings, writes a plain MP4 and prints timings. Used for the phase 1 measurements.
/// </summary>
internal static class DevSelfTest
{
    /// <summary>"--selftest-codec": encodes a synthetic 1280x720 frame in every keyframes.format, decodes it back,
    /// transcodes it for the UI and prints size and timings. Checks the codec DLLs load in the published build.</summary>
    public static unsafe int Codec(string[] args)
    {
        Log.AlsoConsole = true;
        const int w = 1280, h = 720;
        var px = new byte[w * h * 4];
        for (int y = 0; y < h; y++)
            for (int x = 0; x < w; x++)
            {
                int i = (y * w + x) * 4;
                bool text = y % 24 < 12 && x % 9 < 6 && (x / 200 + y / 48) % 3 != 0;
                byte v = text ? (byte)30 : (byte)(200 + (x + y) % 40);
                px[i] = v; px[i + 1] = v; px[i + 2] = (byte)(text ? 30 : 230); px[i + 3] = 255;
            }
        int rc = 0;
        foreach (var format in new[] { "avif", "webp", "jxl", "jpeg" })
        {
            try
            {
                var sw = Stopwatch.StartNew();
                byte[] enc;
                fixed (byte* p = px) enc = ImageCodec.EncodeBgra(p, w, h, w * 4, format, 75);
                double encMs = sw.Elapsed.TotalMilliseconds;
                sw.Restart();
                var (_, dw, dh) = ImageCodec.DecodeBgra(enc, ImageCodec.Mime(format));
                double decMs = sw.Elapsed.TotalMilliseconds;
                var ui = ImageCodec.Transcode(enc, ImageCodec.Mime(format), "webp", 90);
                bool ok = dw == w && dh == h && ui.Width == w;
                if (!ok) rc = 1;
                Console.WriteLine($"{format}: {enc.Length / 1024} KiB, encode {encMs:F0} ms, decode {decMs:F0} ms, ui {ui.Mime} {ui.Bytes.Length / 1024} KiB {(ok ? "ok" : "MISMATCH")}");
            }
            catch (Exception ex)
            {
                rc = 1;
                Console.WriteLine($"{format}: FAILED {ex.GetType().Name}: {ex.Message}");
            }
        }
        // AV1 hardware (iGPU, oneVPL): "--selftest-codec [outDir]" also writes hw.avif there for a WIC / libavif check.
        using var devices = new GpuDeviceCache();
        using var hw = HwAvifEncoder.Create(devices, out var why);
        if (hw is null) Console.WriteLine($"avif hardware: unavailable ({why})");
        else
        {
            try
            {
                for (int i = 0; i < 4; i++)
                {
                    var sw = Stopwatch.StartNew();
                    byte[]? enc;
                    string? err;
                    fixed (byte* p = px) enc = hw.Encode(p, w, h, w * 4, 75, out err);
                    double encMs = sw.Elapsed.TotalMilliseconds;
                    if (enc is null) { rc = 1; Console.WriteLine($"avif hardware: FAILED {err}"); break; }
                    var (_, dw, dh) = ImageCodec.DecodeBgra(enc, "image/avif");
                    bool ok = dw == w && dh == h;
                    if (!ok) rc = 1;
                    Console.WriteLine($"avif hardware ({hw.Name}, QP {HwAvifEncoder.QpFor(75)}): {enc.Length / 1024} KiB, encode {encMs:F0} ms {(ok ? "ok" : $"MISMATCH {dw}x{dh}")}");
                    if (i == 0 && args.Length > 1) File.WriteAllBytes(Path.Combine(args[1], "hw.avif"), enc);
                }
            }
            catch (Exception ex)
            {
                rc = 1;
                Console.WriteLine($"avif hardware: FAILED {ex.GetType().Name}: {ex.Message}");
            }
        }
        return rc;
    }

    /// <summary>"--selftest-bench [seconds-per-run]": the encoder and capture benchmark on this PC, no worker, no Core.</summary>
    public static int Bench(string[] args)
    {
        Log.AlsoConsole = true;
        double perRun = args.Length > 1 && double.TryParse(args[1], System.Globalization.CultureInfo.InvariantCulture, out var s) ? s : 2.5;
        using var devices = new GpuDeviceCache();
        var captures = GpuDevices.Outputs().Select(o => new OutputCapture(o, devices, g => g.Dispose())).ToList();
        using var engine = new ReplayEngine(devices, () => captures, () => []);
        engine.Configure(new ReplayEngineConfig(true, "composited", 30, 6000, 15, "auto", "auto", "native", 100, false));
        foreach (var c in captures) c.Start();
        engine.Start();
        Thread.Sleep(3000);
        var target = new AgentBenchTarget(() => captures, engine, _ => Task.FromResult<BenchResult?>(null), () => new Settings(), devices);
        var report = BenchRunner.RunAsync(target, TimeSpan.FromSeconds(perRun), () => DateTimeOffset.UtcNow, CancellationToken.None).GetAwaiter().GetResult();
        Console.WriteLine($"took {report.TookMs} ms, {report.Encoders.Length} encoder rows");
        foreach (var e in report.Encoders)
            Console.WriteLine($"  {e.Adapter,-40} {e.Codec,-5} {e.Api,-4} {e.Layout,-11} {e.Width}x{e.Height}@{e.Fps} ok={e.Ok} p95={e.EncodeMsP95:F1}ms cpu={e.CpuPercent:F1}% ws={e.WorkingSetMb}MB {e.Error}");
        if (report.Capture is { } c2) Console.WriteLine($"capture tick {c2.TickMs:F2} ms, keyframe encode {c2.KeyframeEncodeMs:F1} ms, cross-adapter copy {c2.CrossAdapterMs:F2} ms");
        engine.Dispose();
        foreach (var c in captures) c.Dispose();
        return 0;
    }

    public static int Replay(string[] args)
    {
        Log.AlsoConsole = true;
        Log.MinLevel = Log.Level.Debug;
        int seconds = args.Length > 1 && int.TryParse(args[1], out var s1) ? s1 : 10;
        string outPath = args.Length > 2 && !args[2].StartsWith("--") ? args[2] : "replay-selftest.mp4";
        int fps = args.Length > 3 && int.TryParse(args[3], out var s3) ? s3 : 15;
        int kbps = args.Length > 4 && int.TryParse(args[4], out var s4) ? s4 : 6000;
        string encoder = args.Length > 5 && !args[5].StartsWith("--") ? args[5] : "auto";
        bool withAudio = !args.Contains("--no-audio");
        bool replayOff = args.Contains("--replay-off");

        string? adapter = null;
        int adapterIdx = Array.IndexOf(args, "--adapter");
        if (adapterIdx >= 0 && adapterIdx + 1 < args.Length) adapter = args[adapterIdx + 1];

        string? res = null;
        int resIdx = Array.IndexOf(args, "--res");
        if (resIdx >= 0 && resIdx + 1 < args.Length) res = args[resIdx + 1];

        string layout = "composited";
        int layoutIdx = Array.IndexOf(args, "--layout");
        if (layoutIdx >= 0 && layoutIdx + 1 < args.Length) layout = args[layoutIdx + 1];

        var adapters = GpuDevices.Adapters();
        foreach (var a in adapters) Console.WriteLine($"adapter vendor=0x{a.VendorId:X4} luid={a.Luid:X} sw={a.Software} {a.Name}");
        var outputs = GpuDevices.Outputs();
        int onlyIdx = Array.IndexOf(args, "--only");
        if (onlyIdx >= 0 && onlyIdx + 1 < args.Length && int.TryParse(args[onlyIdx + 1], out var onlyMon))
            outputs = outputs.Where(o => o.Index == onlyMon).ToList();
        foreach (var o in outputs) Console.WriteLine($"output {o.Index} {o.Width}x{o.Height} at {o.Left},{o.Top} on vendor 0x{o.VendorId:X4}");

        using var devices = new GpuDeviceCache();
        var captures = outputs.Select(o => new OutputCapture(o, devices, g => g.Dispose())).ToList();
        using var engine = new ReplayEngine(devices, () => captures, () => []);
        var cfg = new ReplayEngineConfig(!replayOff, layout, fps, kbps, Math.Max(seconds, 15), encoder, adapter ?? "auto", res ?? "native", 100, false);
        engine.Configure(cfg);

        AudioReplayEncoder? audio = null;
        if (withAudio)
        {
            try { audio = AudioReplayEncoder.Start(cfg.Seconds, lockMemory: false); }
            catch (Exception ex) { Console.WriteLine($"audio unavailable: {ex.Message}"); }
        }

        var proc = Process.GetCurrentProcess();
        foreach (var c in captures) c.Start();
        engine.Start();

        Thread.Sleep(5000); // warm-up excluded from the CPU figure
        proc.Refresh();
        var cpu0 = proc.TotalProcessorTime;
        var sw = Stopwatch.StartNew();
        Thread.Sleep(seconds * 1000);
        proc.Refresh();
        var cpu = proc.TotalProcessorTime - cpu0;
        Console.WriteLine($"cpu {cpu.TotalMilliseconds / sw.Elapsed.TotalMilliseconds * 100:F2}% of one core over {sw.Elapsed.TotalSeconds:F1} s, ws {proc.WorkingSet64 / 1048576} MB, private {proc.PrivateMemorySize64 / 1048576} MB");

        foreach (var c in captures)
        {
            Console.WriteLine($"output {c.Info.Index}: state={c.State} acquires={c.AcquireCount} frames={c.FrameCount} lost={c.AccessLost}");
        }

        foreach (var p in engine.Pipelines)
        {
            double copyMs = p.CopyCount == 0 ? 0 : p.CopyTicks * 1000.0 / Stopwatch.Frequency / p.CopyCount;
            Console.WriteLine($"replay {p.Spec.Key} {p.Encoder.Codec} {p.Encoder.Api} '{p.Encoder.Name}' {p.Encoder.Width}x{p.Encoder.Height}@{p.Spec.Fps}: in {p.Encoder.FramesIn} out {p.Encoder.PacketsOut} dropped {p.Encoder.FramesDropped} busy {p.BusyCount} forcedKeys {p.Encoder.ForcedKeyframes} repeats {p.RepeatCount} copy {copyMs:F2} ms p95 {p.Encoder.Latency.Percentile(95):F2} ms, ring {p.Stream.Ring.CommittedBytes / 1048576.0:F1}/{p.Stream.Ring.Capacity / 1048576.0:F0} MiB, lockFailed {p.Stream.Ring.LockFailed}");
        }
        if (!string.IsNullOrEmpty(engine.LastError))
            Console.WriteLine($"replay error: {engine.LastError}");

        if (audio is not null) Console.WriteLine($"audio: packets={audio.Ring.Count} live={audio.Ring.LiveBytes / 1024} KiB frames={audio.FramesCaptured} gaps={audio.Gaps} silentPackets={audio.SilentPackets}");

        int rc = 0;
        if (engine.Pipelines.Count > 0)
        {
            var save = Stopwatch.StartNew();
            using (var fs = File.Create(outPath))
            {
                int ms = engine.Save(fs, audio?.Source, seconds);
                Console.WriteLine($"saved {outPath}: {ms} ms, {fs.Length / 1024} KiB in {save.ElapsedMilliseconds} ms");
            }
        }
        else rc = 2;

        engine.Dispose();
        audio?.Dispose();
        foreach (var c in captures) c.Dispose();
        return rc;
    }

    public static int Keyframe(string[] args)
    {
        Log.AlsoConsole = true;
        int maxWidth = args.Length > 1 ? int.Parse(args[1]) : 1280;
        string format = args.Length > 2 ? args[2] : "webp";
        int quality = args.Length > 3 ? int.Parse(args[3]) : 75;
        string? outDir = args.Length > 4 ? args[4] : null;
        var outputs = GpuDevices.Outputs();
        using var devices = new GpuDeviceCache();
        int done = 0;
        var encodeMs = new List<double>();
        var hashMs = new List<double>();
        var grabMs = new List<double>();
        using var finished = new ManualResetEventSlim();
        var captures = outputs.Select(o => new OutputCapture(o, devices, g =>
        {
            unsafe
            {
                var sw = Stopwatch.StartNew();
                ulong h = DHash.Compute(g.Span, g.Width, g.Height, g.Stride);
                hashMs.Add(sw.Elapsed.TotalMilliseconds);
                sw.Restart();
                byte[] bytes = ImageCodec.EncodeBgra(g.Pixels, g.Width, g.Height, g.Stride, format, quality);
                encodeMs.Add(sw.Elapsed.TotalMilliseconds);
                Console.WriteLine($"monitor {g.Monitor}: {g.Width}x{g.Height} dhash={h:X16} {format} {bytes.Length / 1024} KiB");
                if (outDir is not null) File.WriteAllBytes(Path.Combine(outDir, $"kf-{g.Monitor}.{(format == "jpeg" ? "jpg" : format)}"), bytes);
                g.Dispose();
                if (Interlocked.Increment(ref done) >= outputs.Count * 3) finished.Set();
            }
        })).ToList();
        foreach (var c in captures) c.Start();
        Thread.Sleep(500);
        for (int round = 0; round < 3; round++)
        {
            foreach (var c in captures)
                c.PendingKeyframe = new KeyframeRequest(DateTimeOffset.UtcNow, "", "", "", "", true, maxWidth, 0);
            Thread.Sleep(700);
        }
        finished.Wait(5000);
        foreach (var c in captures)
        {
            double g = c.GrabCount == 0 ? 0 : c.GrabTicks * 1000.0 / Stopwatch.Frequency / c.GrabCount;
            Console.WriteLine($"output {c.Info.Index}: grabs={c.GrabCount} gpu-scale+readback={g:F2} ms");
            c.Dispose();
        }
        if (encodeMs.Count > 0)
            Console.WriteLine($"encode {format} q{quality}: avg {encodeMs.Average():F1} ms, max {encodeMs.Max():F1} ms; dhash avg {hashMs.Average():F3} ms");
        return 0;
    }
}
