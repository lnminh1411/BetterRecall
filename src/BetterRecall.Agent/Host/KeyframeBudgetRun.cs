// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Collections.Concurrent;
using System.Diagnostics;
using System.Globalization;
using BetterRecall.Agent.Capture;
using BetterRecall.Agent.Native;
using BetterRecall.Agent.Scheduling;
using BetterRecall.Agent.Util;
using BetterRecall.Contracts;

namespace BetterRecall.Agent.Host;

/// <summary>
/// "--selftest-keyframe-budget [minutes]": the keyframe path on the real screens with default settings (every monitor,
/// a grab every capture.intervalMs, window-switch triggers, smart capture, EncodeGovernor, KeyframeEncoder on a
/// below-normal EcoQoS thread). Nothing is stored and no title or image is printed: only numbers. Every 10 s it prints
/// the encode CPU as Task Manager counts it (share of all logical processors); at the end the 60 s average, the 10 s
/// peak, ms per frame per level, and, from a second thread outside the budget, the size WebP and JPEG need to reach the
/// same text sharpness as each AVIF keyframe.
/// </summary>
internal static unsafe class KeyframeBudgetRun
{
    private sealed record Encoded(long T, EncodeLevel Level, double CpuMs, double WallMs, int Bytes);

    private sealed class Item(KeyframeGrab g) : IDisposable
    {
        public KeyframeGrab Grab { get; } = g;
        public void Dispose() => Grab.Dispose();
    }

    public static int Run(string[] args)
    {
        Log.AlsoConsole = true;
        CultureInfo.CurrentCulture = CultureInfo.InvariantCulture;
        double minutes = args.Length > 1 ? double.Parse(args[1], CultureInfo.InvariantCulture) : 10;
        var s = new Settings();
        var outputs = GpuDevices.Outputs();
        int procs = Environment.ProcessorCount;
        Console.WriteLine($"{outputs.Count} monitor(s), {procs} logical processors, {minutes} min, interval {s.Capture.IntervalMs} ms, " +
                          $"format {s.Keyframes.Format} q{s.Keyframes.Quality}, dirtyMinPercent {s.Capture.DirtyMinPercent}");
        Avif.TryLoad();

        using var devices = new GpuDeviceCache();
        var grabs = new BlockingCollection<KeyframeGrab>(new ConcurrentQueue<KeyframeGrab>(), 6);
        var compare = new BlockingCollection<(byte[] Px, int W, int H, byte[] Avif)>(4);
        var encodes = new List<Encoded>();
        var matched = new List<(int Avif, int Webp, int Jpeg)>();
        long skipped = 0, kept = 0, channelDrops = 0;
        var clock = Stopwatch.StartNew();

        var captures = outputs.Select(o => new OutputCapture(o, devices, g =>
        {
            if (!grabs.TryAdd(g)) { Interlocked.Increment(ref channelDrops); g.Dispose(); }
        })).ToList();

        EncodeGovernor<Item>? governor = null;
        var worker = new Thread(() =>
        {
            Governance.ApplyEcoQosToCurrentThread();
            var dedup = new Dictionary<int, DedupFilter>();
            governor = new EncodeGovernor<Item>(() => clock.ElapsedMilliseconds, procs, (item, level) =>
            {
                var g = item.Grab;
                double cpu0 = Win32.CurrentThreadCpuMs();
                var sw = Stopwatch.StartNew();
                var (bytes, format, _, _) = KeyframeEncoder.Encode(g.Pixels, g.Width, g.Height, g.Stride, s, level);
                double cpu = Win32.CurrentThreadCpuMs() - cpu0;
                lock (encodes) encodes.Add(new Encoded(clock.ElapsedMilliseconds, level, cpu, sw.Elapsed.TotalMilliseconds, bytes.Length));
                if (format == "avif" && level == EncodeLevel.Normal && compare.Count < 2)
                {
                    var px = new byte[g.Width * g.Height * 4];
                    new ReadOnlySpan<byte>(g.Pixels, px.Length).CopyTo(px);
                    if (!compare.TryAdd((px, g.Width, g.Height, bytes))) Array.Clear(px);
                }
                else Array.Clear(bytes);
                return cpu;
            }, (_, why) => Console.WriteLine($"  dropped ({why})"), KeyframeEncoder.InitialEstimateMs);
            foreach (var g in grabs.GetConsumingEnumerable())
            {
                bool owned = false;
                try
                {
                    if (!dedup.TryGetValue(g.Monitor, out var d)) dedup[g.Monitor] = d = new DedupFilter();
                    ulong hash = DHash.Compute(g.Span, g.Width, g.Height, g.Stride);
                    var map = ChangeMap.Compute(g.Span, g.Width, g.Height, g.Stride);
                    var (reason, _) = d.Decide(map, hash, g.Request, s.Capture.DhashThreshold, s.Capture.DirtyMinPercent);
                    if (reason == KeepReason.Skip) { map.Clear(); skipped++; continue; }
                    d.Accept(hash, g.Request.Context, map);
                    kept++;
                    lock (governor) governor.Offer(new Item(g));
                    owned = true;
                }
                finally { if (!owned) g.Dispose(); }
                lock (governor) governor.Pump();
            }
        }) { IsBackground = true, Priority = ThreadPriority.BelowNormal, Name = "keyframe-budget" };
        worker.Start();

        // Size check outside the budget: smallest WebP / JPEG whose text-block PSNR reaches the AVIF's.
        var comparer = new Thread(() =>
        {
            foreach (var (px, w, h, avif) in compare.GetConsumingEnumerable())
            {
                try
                {
                    var (dec, dw, dh) = ImageCodec.DecodeBgra(avif, "image/avif");
                    if (dw != w || dh != h) continue;
                    double target = TextPsnr(px, dec, w, h);
                    Array.Clear(dec);
                    int Smallest(string format)
                    {
                        foreach (int q in new[] { 40, 50, 60, 70, 75, 80, 85, 90, 95 })
                        {
                            byte[] enc;
                            fixed (byte* p = px) enc = ImageCodec.EncodeBgra(p, w, h, w * 4, format, q);
                            var (d2, _, _) = ImageCodec.DecodeBgra(enc, ImageCodec.Mime(format));
                            double t = TextPsnr(px, d2, w, h);
                            Array.Clear(d2);
                            if (t >= target) return enc.Length;
                        }
                        return -1;
                    }
                    lock (matched) matched.Add((avif.Length, Smallest("webp"), Smallest("jpeg")));
                }
                finally { Array.Clear(px); Array.Clear(avif); }
            }
        }) { IsBackground = true, Priority = ThreadPriority.Lowest, Name = "keyframe-compare" };
        comparer.Start();

        foreach (var c in captures) c.Start();
        Thread.Sleep(500);
        long end = (long)(minutes * 60_000), nextTick = 0, nextReport = 10_000, lastTrigger = -10_000;
        nint lastFg = 0;
        var proc = Process.GetCurrentProcess();
        double procCpu0 = proc.TotalProcessorTime.TotalMilliseconds;
        while (clock.ElapsedMilliseconds < end)
        {
            long now = clock.ElapsedMilliseconds;
            nint fg = Win32.GetForegroundWindow();
            bool trigger = fg != lastFg && now - lastTrigger >= 1000;
            if (fg != lastFg) lastFg = fg;
            if (now >= nextTick || trigger)
            {
                if (now >= nextTick) nextTick = now + s.Capture.IntervalMs;
                if (trigger) lastTrigger = now;
                string ctx = fg.ToString(CultureInfo.InvariantCulture);
                foreach (var c in captures)
                    c.PendingKeyframe = new KeyframeRequest(DateTimeOffset.UtcNow, "", "", "", ctx, false, s.Capture.MaxWidth,
                        Math.Min(s.Capture.DirtyMinPercent, CapturePolicy.TriggerMinScore), "", Trigger: trigger);
            }
            if (now >= nextReport)
            {
                nextReport += 10_000;
                double cpu10;
                int[] perLevel = new int[4];
                lock (encodes)
                {
                    cpu10 = encodes.Where(e => e.T > now - 10_000).Sum(e => e.CpuMs);
                    foreach (var e in encodes.Where(e => e.T > now - 10_000)) perLevel[(int)e.Level]++;
                }
                Console.WriteLine(FormattableString.Invariant(
                    $"t={now / 1000,4} s  encode CPU last 10 s {cpu10 / (procs * 10_000.0) * 100:F2} %  frames n/f/r/w {perLevel[0]}/{perLevel[1]}/{perLevel[2]}/{perLevel[3]}  waiting {governor?.Waiting ?? 0}  kept {kept} skipped {skipped}"));
            }
            Thread.Sleep(50);
        }
        foreach (var c in captures) c.Dispose();
        grabs.CompleteAdding();
        worker.Join(10_000);
        compare.CompleteAdding();
        comparer.Join(120_000);
        proc.Refresh();
        double procCpu = proc.TotalProcessorTime.TotalMilliseconds - procCpu0;

        // Windows over the records: 10 s and 60 s sliding, evaluated at every encode.
        var list = encodes.ToList();
        double peak10 = 0, peak60 = 0;
        foreach (var e in list)
        {
            peak10 = Math.Max(peak10, list.Where(x => x.T > e.T - 10_000 && x.T <= e.T).Sum(x => x.CpuMs));
            peak60 = Math.Max(peak60, list.Where(x => x.T > e.T - 60_000 && x.T <= e.T).Sum(x => x.CpuMs));
        }
        double total = list.Sum(e => e.CpuMs);
        Console.WriteLine();
        Console.WriteLine(FormattableString.Invariant($"run {end / 1000} s: {kept} kept, {skipped} skipped as similar, {list.Count} encoded, {governor?.Dropped ?? 0} dropped by budget, {channelDrops} grab drops"));
        Console.WriteLine(FormattableString.Invariant($"encode CPU: average {total / (procs * (double)end) * 100:F3} %, peak 10 s {peak10 / (procs * 10_000.0) * 100:F3} % (cap 1.5), peak 60 s {peak60 / (procs * 60_000.0) * 100:F3} % (cap 1.0)"));
        Console.WriteLine(FormattableString.Invariant($"whole test process CPU (capture, smart capture, size check included): {procCpu / (procs * (double)end) * 100:F3} %"));
        foreach (var g in list.GroupBy(e => e.Level).OrderBy(g => g.Key))
            Console.WriteLine(FormattableString.Invariant(
                $"  {g.Key,-8} {g.Count(),4} frames  CPU {g.Average(e => e.CpuMs):F1} ms (max {g.Max(e => e.CpuMs):F1})  wall {g.Average(e => e.WallMs):F1} ms  {g.Average(e => e.Bytes) / 1024:F1} KiB"));
        if (matched.Count > 0)
        {
            var ok = matched.Where(m => m.Webp > 0 && m.Jpeg > 0).ToList();
            Console.WriteLine(string.Concat(
                $"matched text sharpness ({matched.Count} frames, {ok.Count} reachable by both): AVIF {matched.Average(m => m.Avif) / 1024:F1} KiB, " +
                $"WebP {(ok.Count > 0 ? ok.Average(m => m.Webp) / 1024 : 0):F1} KiB, JPEG {(ok.Count > 0 ? ok.Average(m => m.Jpeg) / 1024 : 0):F1} KiB" +
                $" (WebP unreachable {matched.Count(m => m.Webp < 0)}, JPEG unreachable {matched.Count(m => m.Jpeg < 0)})"));
        }
        return 0;
    }

    /// <summary>Luma PSNR over 16x16 blocks whose source luma range is at least 64 (text and edges).</summary>
    internal static double TextPsnr(byte[] src, byte[] dec, int w, int h)
    {
        double sum = 0;
        long n = 0;
        for (int by = 0; by < h; by += 16)
            for (int bx = 0; bx < w; bx += 16)
            {
                int bh = Math.Min(16, h - by), bw = Math.Min(16, w - bx);
                double min = 255, max = 0;
                for (int y = by; y < by + bh; y++)
                    for (int x = bx; x < bx + bw; x++)
                    {
                        int i = (y * w + x) * 4;
                        double l = 0.2126 * src[i + 2] + 0.7152 * src[i + 1] + 0.0722 * src[i];
                        if (l < min) min = l;
                        if (l > max) max = l;
                    }
                if (max - min < 64) continue;
                for (int y = by; y < by + bh; y++)
                    for (int x = bx; x < bx + bw; x++)
                    {
                        int i = (y * w + x) * 4;
                        double d = 0.2126 * (src[i + 2] - dec[i + 2]) + 0.7152 * (src[i + 1] - dec[i + 1]) + 0.0722 * (src[i] - dec[i]);
                        sum += d * d;
                        n++;
                    }
            }
        return n == 0 || sum == 0 ? 99 : 10 * Math.Log10(255.0 * 255.0 / (sum / n));
    }
}
