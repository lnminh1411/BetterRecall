// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using BetterRecall.Agent.Audio;
using Xunit;

namespace BetterRecall.Agent.Tests;

public class AudioTimelineTests
{
    private const int Rate = 48000;
    private const int Packet = 480;                  // 10 ms
    private const long PacketTime = 100_000;         // 10 ms in 100 ns

    private static AudioTimeline.Step Run(AudioTimeline tl, long qpc, bool silent, int frames = Packet)
    {
        var s = tl.Plan(qpc, frames, silent);
        tl.Commit(s, qpc);
        return s;
    }

    [Fact]
    public void QuietPacketsInsideRunKeepTheClock()
    {
        var tl = new AudioTimeline(Rate);
        long t = 1_000_000_000;
        for (int i = 0; i < 300; i++) Run(tl, t + i * PacketTime, silent: i % 3 == 1); // every third packet quiet
        // 3 s captured -> 3 s on the timeline: quiet packets are encoded as zeros, never collapsed.
        Assert.Equal(t + 300 * PacketTime, tl.Expected);
        Assert.Equal(1, tl.Runs);
    }

    [Fact]
    public void MissingPacketsAreFilledNotCollapsed()
    {
        var tl = new AudioTimeline(Rate);
        long t = 5_000_000;
        Run(tl, t, false);
        var s = Run(tl, t + 5 * PacketTime, false);   // 40 ms of packets never arrived
        Assert.Equal(4 * Packet, s.FillFrames);
        Assert.Equal(t + 6 * PacketTime, tl.Expected);
    }

    [Fact]
    public void JitterUnderTwoMillisecondsIsContinuous()
    {
        var tl = new AudioTimeline(Rate);
        Run(tl, 0, false);
        var s = Run(tl, PacketTime + 15_000, false);  // 1.5 ms late
        Assert.Equal(0, s.FillFrames);
        Assert.Equal(0, s.SkipFrames);
        Assert.Equal(2 * PacketTime, tl.Expected);
    }

    [Fact]
    public void OverlapFromDriftIsTrimmed()
    {
        var tl = new AudioTimeline(Rate);
        Run(tl, 0, false);
        var s = Run(tl, PacketTime - 50_000, false);  // 5 ms early
        Assert.Equal(240, s.SkipFrames);
        Assert.Equal(240, s.FeedFrames);
        Assert.Equal(PacketTime + 50_000, tl.Expected);
    }

    [Fact]
    public void LongSilenceEndsRunAndSoundStartsANewOneAtItsOwnTime()
    {
        var tl = new AudioTimeline(Rate);
        long t = 0;
        Run(tl, t, false);
        int i = 1;
        for (; i <= 250; i++)
        {
            var s = Run(tl, t + i * PacketTime, true);
            if (s.EndRun) break;
        }
        Assert.False(tl.InRun);
        Assert.InRange(i, 200, 202);                  // ends after ~2 s of quiet
        Assert.Equal(0, tl.Plan(t + 300 * PacketTime, Packet, true).FeedFrames); // quiet with no run: nothing
        long resume = t + 900 * PacketTime;
        var start = Run(tl, resume, false);
        Assert.True(start.StartRun);
        Assert.Equal(resume, tl.RunStart);
        Assert.Equal(2, tl.Runs);
    }

    [Fact]
    public void ClockJumpStartsNewRun()
    {
        var tl = new AudioTimeline(Rate);
        Run(tl, 0, false);
        var s = Run(tl, 50_000_000, false);           // 5 s later, loud
        Assert.True(s.EndRun);
        Assert.True(s.StartRun);
        Assert.Equal(50_000_000, tl.RunStart);
    }

    [Fact]
    public void LeadingSilenceIsNotEncoded()
    {
        var tl = new AudioTimeline(Rate);
        var s = Run(tl, 0, true);
        Assert.False(tl.InRun);
        Assert.Equal(0, s.FeedFrames);
    }
}
