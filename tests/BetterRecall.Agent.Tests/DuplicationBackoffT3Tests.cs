// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using BetterRecall.Agent.Capture;
using BetterRecall.Agent.Util;

namespace BetterRecall.Agent.Tests;

/// <summary>T3: in Windows Sandbox (vGPU) every new duplication got ACCESS_LOST on its first AcquireNextFrame, so the
/// capture thread re-created it every ~1.04 s (58 a minute). The capture loop's retry decision, driven with a fake clock
/// the way OutputCapture.Run drives it (500 ms sleeps while no duplication is up).</summary>
public class DuplicationBackoffT3Tests
{
    private const long Start = 10_000;

    /// <summary>Times at which a duplication comes up in one minute when each one is lost lostAfterMs after it came up,
    /// having delivered framesBeforeLoss frames.</summary>
    private static List<long> Ups(long minutes, long lostAfterMs, long framesBeforeLoss, bool backoff = true)
    {
        var barren = new RetryBackoff(1000, OutputCapture.BarrenMaxMs);
        long now = Start, lastTry = 0;
        var ups = new List<long>();
        while (now < Start + minutes * 60_000)
        {
            if (OutputCapture.RetryDue(now, lastTry, backoff ? barren.NextAt : long.MinValue))
            {
                lastTry = now;
                ups.Add(now);
                now += lostAfterMs;
                if (OutputCapture.Barren(framesBeforeLoss, lostAfterMs)) barren.Failed(now);
                else barren.Succeeded();
            }
            else now += 500;
        }
        return ups;
    }

    [Fact]
    public void Lost_before_the_first_frame_is_barren_a_static_desktop_is_not()
    {
        Assert.True(OutputCapture.Barren(0, 5));                                // sandbox: lost on the first acquire
        Assert.True(OutputCapture.Barren(0, 255));                              // replay idle: first acquire after 250 ms
        Assert.False(OutputCapture.Barren(1, 5));                               // delivered a frame
        Assert.False(OutputCapture.Barren(0, OutputCapture.BarrenMs));          // quiet desktop, up for 5 s
        Assert.False(OutputCapture.Barren(1200, 3_600_000));                    // ran for an hour, then a desktop switch
    }

    [Fact]
    public void Retry_waits_one_second_after_the_last_try_and_for_the_backoff()
    {
        Assert.False(OutputCapture.RetryDue(Start + 999, Start, long.MinValue));
        Assert.True(OutputCapture.RetryDue(Start + 1000, Start, long.MinValue));
        Assert.False(OutputCapture.RetryDue(Start + 1000, Start, Start + 4000));
        Assert.True(OutputCapture.RetryDue(Start + 4000, Start, Start + 4000));
    }

    /// <summary>The loop seen in the sandbox: without the backoff ~1 re-creation a second, with it the waits grow to
    /// BarrenMaxMs, so a dead output costs a handful of re-creations a minute.</summary>
    [Fact]
    public void Barren_duplications_back_off_to_the_cap()
    {
        var old = Ups(1, 5, 0, backoff: false);
        Assert.InRange(old.Count, 55, 61);

        var ups = Ups(3, 5, 0);
        Assert.InRange(ups.Count(t => t < Start + 60_000), 6, 9);
        var gaps = ups.Zip(ups.Skip(1), (a, b) => b - a).ToList();
        Assert.All(gaps.Zip(gaps.Skip(1)), p => Assert.True(p.Second >= p.First, $"gap shrank: {p.First} -> {p.Second}"));
        Assert.All(gaps.Skip(5), g => Assert.InRange(g, OutputCapture.BarrenMaxMs, OutputCapture.BarrenMaxMs + 500));
    }

    /// <summary>Real hardware: a duplication that ran and delivered frames is lost to a desktop switch or a mode change.
    /// It is re-created after 1 s, as before, however often that happens.</summary>
    [Fact]
    public void A_duplication_that_delivered_frames_is_retried_after_one_second()
    {
        var ups = Ups(1, 300, 10);   // lost 300 ms after coming up, after 10 frames
        var gaps = ups.Zip(ups.Skip(1), (a, b) => b - a).ToList();
        Assert.NotEmpty(gaps);
        Assert.All(gaps, g => Assert.InRange(g, 1000, 1500));
    }

    /// <summary>A healthy duplication ends the barren run: the next loss retries after 1 s, not the backed-off wait.</summary>
    [Fact]
    public void A_frame_resets_the_backoff()
    {
        var barren = new RetryBackoff(1000, OutputCapture.BarrenMaxMs);
        long now = Start;
        var delays = new List<long>();
        for (int i = 0; i < 6; i++) { now += 5; if (OutputCapture.Barren(0, 5)) delays.Add(barren.Failed(now)); now = barren.NextAt; }
        Assert.Equal([1000, 2000, 4000, 8000, OutputCapture.BarrenMaxMs, OutputCapture.BarrenMaxMs], delays);
        now += 20_000;
        if (OutputCapture.Barren(500, 20_000)) barren.Failed(now); else barren.Succeeded();
        Assert.True(OutputCapture.RetryDue(now + 1000, now - 20_000, barren.NextAt));
    }
}
