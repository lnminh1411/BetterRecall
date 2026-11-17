// SPDX-License-Identifier: MIT
// Copyright (c) BetterRecall contributors
using System.Diagnostics;
using BetterRecall.Contracts;
using BetterRecall.UI.Logic;

namespace BetterRecall.UI.Tests;

/// <summary>Bug sweep 4, batch L (UI client side): T2, the agent stays trusted after the clock steps back.</summary>
public class Sweep4LUiTests
{
    [Fact]
    public void T2_StartBound_follows_the_boot_clock_only_after_a_step_back()
    {
        var wall = new DateTime(2026, 11, 16, 11, 51, 52, DateTimeKind.Utc);
        long up = 3_600_000;
        Assert.Equal(wall, ServerTrustVerifier.StartBound(wall, up, wall.AddMilliseconds(-up).AddSeconds(3)));
        var boot = wall.AddMilliseconds(-up).AddHours(2);
        Assert.Equal(boot.AddMilliseconds(up), ServerTrustVerifier.StartBound(wall, up, boot));
        Assert.Equal(wall, ServerTrustVerifier.StartBound(wall, up, wall.AddMilliseconds(-up).AddHours(-2)));
        Assert.Equal(wall, ServerTrustVerifier.StartBound(wall, up, null));
    }

    [Fact]
    public void S6_Boot_bound_ends_when_the_clock_catches_up()
    {
        var steps = new List<TimeSpan>();
        var clock = new StepClock(steps.Add);
        var boot = new DateTime(2026, 11, 16, 9, 0, 0, DateTimeKind.Utc);
        long ms = 3_600_000;
        var wall = boot.AddMilliseconds(ms);
        Assert.Equal(wall, clock.Bound(wall, ms, boot));
        var before = wall;
        wall -= TimeSpan.FromHours(2);
        Assert.Equal(before, clock.Bound(wall, ms, boot));
        ms += 7_201_000;
        wall += TimeSpan.FromMilliseconds(7_201_000);
        Assert.Equal(wall, clock.Bound(wall, ms, boot));   // caught up, boot clock still 2 h ahead
        Assert.Single(steps);
    }

    [Fact]
    public async Task T2_Agent_started_before_a_2h_step_back_is_still_trusted_and_pid_reuse_is_still_refused()
    {
        await using var server = new FakeLineServer(_ => Envelope.Ok("null"));
        // Start times stamped before the step read 2 h ahead of today's clock: the agent's and the boot's alike.
        var created = Process.GetCurrentProcess().StartTime.ToUniversalTime().AddHours(2);
        DateTime SteppedBound() => ServerTrustVerifier.StartBound(DateTime.UtcNow, Environment.TickCount64,
            DateTime.UtcNow.AddMilliseconds(-Environment.TickCount64).AddHours(2));
        await using (var c = new AgentClient(server.Name, new ServerTrustVerifier(Environment.ProcessPath!, startedOf: _ => created, startBound: SteppedBound)))
            Assert.True((await c.SendAsync("ping")).Ok);
        // No step on the boot clock: a server "started 2 h from now" is a reused pid, as before (B14).
        await using (var c = new AgentClient(server.Name, new ServerTrustVerifier(Environment.ProcessPath!, startedOf: _ => created)))
            Assert.Equal(AgentErrorKind.NotTrusted, (await c.SendAsync("ping")).Kind);
    }
}
