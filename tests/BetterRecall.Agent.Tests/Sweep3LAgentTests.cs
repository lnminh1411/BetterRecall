// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using BetterRecall.Agent.Host;

namespace BetterRecall.Agent.Tests;

/// <summary>Bug sweep 3, batch L: resume log after a timed pause (T6), log timestamps (D19).</summary>
public sealed class Sweep3LAgentTests
{
    [Theory]
    [InlineData("has")]
    [InlineData("continuous")]
    [InlineData("current")]
    public void Expired_timed_pause_is_reported_once_whichever_call_sees_it(string via)
    {
        var now = DateTimeOffset.UnixEpoch;
        var p = new PauseController(() => now);
        var expired = new List<string>();
        p.Expired += expired.Add;
        p.Set("ipc", now.AddMinutes(60));
        p.Set("fullscreen");
        Assert.True(p.Has("ipc"));
        Assert.Empty(expired);

        now = now.AddMinutes(61);
        switch (via)
        {
            case "has": Assert.False(p.Has("ipc")); break;
            case "continuous": Assert.True(p.ContinuousPaused(duringFullscreen: false)); break;   // fullscreen still holds
            default: Assert.Equal("fullscreen", p.Current().Reason); break;
        }
        Assert.Equal(["ipc"], expired);

        p.Current();
        p.Has("ipc");
        Assert.Single(expired);   // once, not on every later call
    }

    [Fact]
    public void Manual_clear_is_not_an_expiry()
    {
        var p = new PauseController();
        var expired = new List<string>();
        p.Expired += expired.Add;
        p.Set("user", DateTimeOffset.UtcNow.AddHours(1));
        p.Clear("user");
        p.Current();
        Assert.Empty(expired);
    }

    [Fact]
    public void Agent_log_stamps_local_time_with_offset()
    {
        var t = new DateTimeOffset(2026, 11, 16, 4, 5, 6, 789, TimeSpan.FromHours(7));
        Assert.Equal("2026-11-16T04:05:06.789+07:00", BetterRecall.Agent.Util.Log.Stamp(t));
    }
}
