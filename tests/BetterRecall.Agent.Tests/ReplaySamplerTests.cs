// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using BetterRecall.Agent.Replay;
using Xunit;

namespace BetterRecall.Agent.Tests;

public sealed class ReplaySamplerTests
{
    private static readonly DateTimeOffset T0 = new(2026, 11, 12, 10, 0, 0, TimeSpan.Zero);
    private DateTimeOffset _now = T0;

    private ReplaySampler New() => new(() => _now);

    private static ReplaySampler.Sample S(DateTimeOffset ts, int mon = 0, byte mark = 1) =>
        new(ts, mon, "game.exe", "Boss", "", 1280, 720, [mark, mark, mark], "image/webp");

    [Fact]
    public void Due_per_monitor_with_jitter_slack()
    {
        var r = New();
        Assert.True(r.Due(0, 5));
        r.Add(S(_now), 60);
        Assert.False(r.Due(0, 5));
        Assert.True(r.Due(1, 5));
        _now += TimeSpan.FromMilliseconds(4700);   // timer fired a little early
        Assert.True(r.Due(0, 5));
    }

    [Fact]
    public void Add_copies_bytes_and_clear_zeroes()
    {
        var r = New();
        var bytes = new byte[] { 7, 7 };
        r.Add(S(_now) with { Bytes = bytes }, 60);
        bytes[0] = 0;
        Assert.Equal(7, r.Take(T0, T0, 5).Single().Bytes[0]);
        r.Clear();
        Assert.Equal(0, r.Count);
        Assert.True(r.Due(0, 5));
    }

    [Fact]
    public void Trim_drops_old_samples_but_keeps_newest_old_one_per_monitor()
    {
        var r = New();
        for (int i = 0; i < 10; i++) r.Add(S(T0.AddSeconds(i * 5), 0, (byte)i), 120);
        r.Add(S(T0, 1, 99), 120);
        Assert.Equal(11, r.Count);
        _now = T0.AddSeconds(200);          // all are older than 120 s
        r.Add(S(_now, 0, 50), 120);
        // monitor 0: newest old (i=9) + new one; monitor 1: its only old one
        Assert.Equal(3, r.Count);
    }

    [Fact]
    public void Take_returns_window_and_carries_in_still_screen()
    {
        var r = New();
        r.Add(S(T0, 0, 1), 300);                     // before the clip
        r.Add(S(T0.AddSeconds(30), 0, 2), 300);      // inside, but late: carry-in for monitor 0
        r.Add(S(T0.AddSeconds(5), 1, 3), 300);       // before the clip on monitor 1
        r.Add(S(T0.AddSeconds(21), 1, 4), 300);      // inside, early: no carry-in for monitor 1
        r.Add(S(T0.AddSeconds(100), 0, 5), 300);     // after the clip
        var start = T0.AddSeconds(20);
        var got = r.Take(start, T0.AddSeconds(60), 5);
        Assert.Equal([(start, 0, (byte)1), (T0.AddSeconds(21), 1, (byte)4), (T0.AddSeconds(30), 0, (byte)2)],
            got.Select(s => (s.Ts, s.Monitor, s.Bytes[0])));
    }

    [Fact]
    public void ToFrames_offsets_are_clamped_to_the_clip()
    {
        var start = T0.AddSeconds(10);
        var frames = ReplaySampler.ToFrames("r1", start, 30000,
            [S(start), S(start.AddSeconds(12.5)), S(start.AddSeconds(40)), S(start.AddSeconds(-1))]).ToList();
        Assert.Equal([0, 12500, 30000, 0], frames.Select(f => f.OffsetMs));
        Assert.All(frames, f => Assert.Equal("r1", f.ReplayId));
        Assert.Equal("Boss", frames[0].Title);
        Assert.Equal("image/webp", frames[0].ImageMime);
    }
}
