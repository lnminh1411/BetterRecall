// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Diagnostics;
using BetterRecall.Agent.Capture;
using BetterRecall.Agent.Native;
using Xunit.Abstractions;

namespace BetterRecall.Agent.Tests;

/// <summary>Bug sweep 3, batch K (reliability): encode budget items A12 and A6.</summary>
public class EncodeBudgetSweep3Tests
{
    private sealed class Item : IDisposable { public void Dispose() { } }

    /// <summary>A12: one slow Normal encode (1500 ms on 4 logical processors, 10 s target 480 ms) used to push the Normal
    /// estimate over the target for the life of the process. An unused level relaxes back and is measured again.</summary>
    [Fact]
    public void One_slow_encode_does_not_disable_a_level_for_good()
    {
        long now = 0;
        var costs = new Queue<double>([1500]);
        var g = new EncodeGovernor<Item>(() => now, 4, (_, _) => costs.Count > 0 ? costs.Dequeue() : 130, (_, _) => { }, KeyframeEncoder.InitialEstimateMs);
        g.Offer(new Item());
        Assert.True(g.Estimate(EncodeLevel.Normal) > 0.8 * g.Limit10Ms);
        var levels = new List<EncodeLevel>();
        for (int i = 0; i < 5; i++) { now += 120_000; g.Offer(new Item()); levels.Add(g.Level); }
        Assert.Contains(EncodeLevel.Normal, levels);
        Assert.Equal(EncodeLevel.Normal, levels[^1]);
    }

    /// <summary>A level still in use keeps its learned estimate; only the excess of an unused one decays.</summary>
    [Fact]
    public void Relax_only_lowers_estimates_of_unused_levels_toward_their_initial_value()
    {
        long now = 0;
        var g = new EncodeGovernor<Item>(() => now, 20, (_, _) => 50, (_, _) => { }, [130, 65, 15, 35]);
        g.Offer(new Item());   // Normal measured cheaper than its initial estimate
        double learned = g.Estimate(EncodeLevel.Normal);
        Assert.True(learned < 130);
        now += 10 * EncodeGovernor<Item>.RelaxAfterMs;
        g.Choose();
        Assert.Equal(learned, g.Estimate(EncodeLevel.Normal));   // below the initial value: kept
    }
}

/// <summary>A6: libjxl must run on the calling thread, the one EncodeGovernor charges. Runs alone so that other tests
/// do not add to the process CPU.</summary>
[CollectionDefinition("JxlCpu", DisableParallelization = true)]
public class JxlCpuCollection;

[Collection("JxlCpu")]
public unsafe class JxlBudgetTests(ITestOutputHelper output)
{
    [Fact]
    public void Jxl_encode_cpu_is_on_the_calling_thread()
    {
        if (!Jxl.TryLoad()) { output.WriteLine("jxl not loadable: " + Jxl.LoadError); return; }
        int w = 1280, h = 720;
        var px = new byte[w * h * 4];
        new Random(1).NextBytes(px);
        for (int i = 3; i < px.Length; i += 4) px[i] = 255;
        fixed (byte* p = px)
        {
            Jxl.Encode(p, w, h, w * 4, 75, 3);   // warm
            using var proc = Process.GetCurrentProcess();
            double t0 = Win32.CurrentThreadCpuMs();
            var p0 = proc.TotalProcessorTime;
            var sw = Stopwatch.StartNew();
            for (int i = 0; i < 5; i++) Jxl.Encode(p, w, h, w * 4, 75, 3);
            double wall = sw.Elapsed.TotalMilliseconds;
            double thread = Win32.CurrentThreadCpuMs() - t0;
            proc.Refresh();
            double process = (proc.TotalProcessorTime - p0).TotalMilliseconds;
            output.WriteLine($"JXL effort 3, 5 encodes at {w}x{h}: calling-thread CPU {thread:F0} ms, process CPU {process:F0} ms, wall {wall:F0} ms");
            Assert.True(thread >= process * 0.5, $"thread {thread:F0} ms vs process {process:F0} ms");
        }
    }
}

/// <summary>T3: a new duplication of an unchanged output keeps its frame textures, so the replay does not rebuild.</summary>
public class DuplicationResetSweep3Tests
{
    [Fact]
    public void Same_sizes_and_rotation_reuse_the_frames()
    {
        Assert.True(OutputCapture.SameFrames(true, (1520, 823, 1520, 823, 0), (1520, 823, 1520, 823, 0)));
        Assert.False(OutputCapture.SameFrames(false, (1520, 823, 1520, 823, 0), (1520, 823, 1520, 823, 0)));   // first start
        Assert.False(OutputCapture.SameFrames(true, (1520, 823, 1520, 823, 0), (1920, 1080, 1920, 1080, 0)));  // mode change
        Assert.False(OutputCapture.SameFrames(true, (1080, 1920, 1920, 1080, 90), (1080, 1920, 1920, 1080, 0)));   // rotation
    }
}
