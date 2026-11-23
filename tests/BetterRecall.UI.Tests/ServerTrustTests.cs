// SPDX-License-Identifier: MIT
// Copyright (c) BetterRecall contributors
using System.Diagnostics;
using BetterRecall.Contracts;
using BetterRecall.UI.Logic;

namespace BetterRecall.UI.Tests;

public sealed class ServerTrustTests
{
    [Theory]
    [InlineData("other-session")]
    [InlineData("unknown-pid")]
    [InlineData("image-lookup-fails")]
    [InlineData("newer-than-connection")]
    public async Task Untrusted_server_hook_is_rejected_and_nothing_is_sent(string mode)
    {
        await using var server = new FakeLineServer(_ => Envelope.Ok("null"));
        ServerTrustVerifier trust = mode switch
        {
            "other-session" => new(Environment.ProcessPath!, sessionOf: _ => 99, ownSession: 1),
            "unknown-pid" => new(Environment.ProcessPath!, serverPid: _ => null),
            "image-lookup-fails" => new(Environment.ProcessPath!, imageOf: _ => null),
            "newer-than-connection" => new(Environment.ProcessPath!, startedOf: _ => DateTime.UtcNow.AddMinutes(5)),
            _ => throw new ArgumentOutOfRangeException(nameof(mode)),
        };
        await using var c = new AgentClient(server.Name, trust);
        Assert.Equal(AgentErrorKind.NotTrusted, (await c.SendAsync("ping")).Kind);
        lock (server.Received) Assert.Empty(server.Received);
    }

    /// <summary>T2: start bound follows the boot clock only after a backward step.</summary>
    [Fact]
    public void StartBound_follows_the_boot_clock_only_after_a_step_back()
    {
        var wall = new DateTime(2026, 11, 16, 11, 51, 52, DateTimeKind.Utc);
        long up = 3_600_000;
        Assert.Equal(wall, ServerTrustVerifier.StartBound(wall, up, wall.AddMilliseconds(-up).AddSeconds(3)));
        var boot = wall.AddMilliseconds(-up).AddHours(2);
        Assert.Equal(boot.AddMilliseconds(up), ServerTrustVerifier.StartBound(wall, up, boot));
        Assert.Equal(wall, ServerTrustVerifier.StartBound(wall, up, wall.AddMilliseconds(-up).AddHours(-2)));
        Assert.Equal(wall, ServerTrustVerifier.StartBound(wall, up, null));
    }

    /// <summary>S6 + C3: StepClock ends the boot bound on catch-up, keeps _until monotonic across multiple backward steps,
    /// and accumulates sub-tolerance backward steps.</summary>
    [Fact]
    public void StepClock_bound_handles_catchup_multiple_steps_and_sub_tolerance()
    {
        // Catch-up: boot bound ends once wall clock catches up to the pre-step time.
        var catchupSteps = new List<TimeSpan>();
        var catchup = new StepClock(catchupSteps.Add);
        var boot1 = new DateTime(2026, 11, 16, 9, 0, 0, DateTimeKind.Utc);
        long ms1 = 3_600_000;
        var wall1 = boot1.AddMilliseconds(ms1);
        Assert.Equal(wall1, catchup.Bound(wall1, ms1, boot1));
        var before = wall1;
        wall1 -= TimeSpan.FromHours(2);
        Assert.Equal(before, catchup.Bound(wall1, ms1, boot1));
        ms1 += 7_201_000;
        wall1 += TimeSpan.FromMilliseconds(7_201_000);
        Assert.Equal(wall1, catchup.Bound(wall1, ms1, boot1));   // caught up, boot clock still 2 h ahead
        Assert.Single(catchupSteps);

        // Multiple backward steps: second step back (-2 min) keeps _until = 12:05 rather than shrinking to 11:10.
        var multiSteps = new List<TimeSpan>();
        var multi = new StepClock(multiSteps.Add);
        var boot2 = new DateTime(2026, 11, 18, 12, 0, 0, DateTimeKind.Utc);
        long ms2 = 60_000;
        var wall2 = boot2.AddMilliseconds(ms2);   // 12:01
        Assert.Equal(wall2, multi.Bound(wall2, ms2, boot2));

        ms2 += 240_000;
        wall2 = new DateTime(2026, 11, 18, 11, 5, 0, DateTimeKind.Utc);
        Assert.Equal(new DateTime(2026, 11, 18, 12, 5, 0, DateTimeKind.Utc), multi.Bound(wall2, ms2, boot2));

        ms2 += 300_000;
        wall2 = new DateTime(2026, 11, 18, 11, 8, 0, DateTimeKind.Utc);
        Assert.Equal(new DateTime(2026, 11, 18, 12, 5, 0, DateTimeKind.Utc), multi.Bound(wall2, ms2, boot2));
        Assert.Equal(2, multiSteps.Count);

        // Sub-10 s backward steps (-6 s x 2) accumulate until StepTolerance is exceeded.
        var subSteps = new List<TimeSpan>();
        var subClock = new StepClock(subSteps.Add);
        var subWall = new DateTime(2026, 11, 18, 12, 1, 0, DateTimeKind.Utc);
        Assert.Equal(subWall, subClock.Bound(subWall, 60_000, boot2));
        Assert.Equal(subWall.AddSeconds(-6), subClock.Bound(subWall.AddSeconds(-6), 60_000, boot2));
        Assert.Empty(subSteps);
        Assert.Equal(subWall, subClock.Bound(subWall.AddSeconds(-12), 60_000, boot2));
        Assert.Single(subSteps);
    }

    /// <summary>T2 + B14: an agent started before a 2 h step back stays trusted, while PID reuse is still refused.</summary>
    [Fact]
    public async Task Agent_started_before_a_2h_step_back_is_still_trusted_and_pid_reuse_is_still_refused()
    {
        await using var server = new FakeLineServer(_ => Envelope.Ok("null"));
        var created = Process.GetCurrentProcess().StartTime.ToUniversalTime().AddHours(2);
        DateTime SteppedBound() => ServerTrustVerifier.StartBound(DateTime.UtcNow, Environment.TickCount64,
            DateTime.UtcNow.AddMilliseconds(-Environment.TickCount64).AddHours(2));
        await using (var c = new AgentClient(server.Name, new ServerTrustVerifier(Environment.ProcessPath!, startedOf: _ => created, startBound: SteppedBound)))
            Assert.True((await c.SendAsync("ping")).Ok);
        // No step on the boot clock: a server "started 2 h from now" is a reused pid, as before (B14).
        await using (var c = new AgentClient(server.Name, new ServerTrustVerifier(Environment.ProcessPath!, startedOf: _ => created)))
            Assert.Equal(AgentErrorKind.NotTrusted, (await c.SendAsync("ping")).Kind);
    }

    /// <summary>B8: expected image path through a directory junction matches the real process image.</summary>
    [Fact]
    public async Task ServerTrustVerifier_resolves_junction_expected_image_against_real_process_image()
    {
        var baseTmp = Environment.GetEnvironmentVariable("BR_TEST_TMP") is { Length: > 0 } t ? t : Path.GetTempPath();
        var root = Path.Combine(baseTmp, "br-ui-b8-" + Guid.NewGuid().ToString("N")[..10]);
        Directory.CreateDirectory(root);
        var junc = Path.Combine(root, "junc");
        var self = Environment.ProcessPath!;
        try
        {
            var psi = new ProcessStartInfo("cmd.exe")
            {
                UseShellExecute = false,
                CreateNoWindow = true,
            };
            psi.ArgumentList.Add("/c");
            psi.ArgumentList.Add("mklink");
            psi.ArgumentList.Add("/J");
            psi.ArgumentList.Add(junc);
            psi.ArgumentList.Add(Path.GetDirectoryName(self)!);
            using var p = Process.Start(psi)!;
            Assert.True(p.WaitForExit(5000), "mklink /J did not exit within 5 s");
            Assert.Equal(0, p.ExitCode);

            await using var server = new FakeLineServer(_ => Envelope.Ok("{\"pong\":true}"));
            var juncExe = Path.Combine(junc, Path.GetFileName(self));
            await using (var ok = new AgentClient(server.Name, new ServerTrustVerifier(juncExe)))
                Assert.True((await ok.SendAsync("ping")).Ok);

            var wrongExe = Path.Combine(junc, "Other.exe");
            await using (var bad = new AgentClient(server.Name, new ServerTrustVerifier(wrongExe)))
                Assert.Equal(AgentErrorKind.NotTrusted, (await bad.SendAsync("ping")).Kind);
        }
        finally
        {
            try { if (Directory.Exists(junc)) Directory.Delete(junc, recursive: false); } catch (IOException) { } catch (UnauthorizedAccessException) { }
            try { if (Directory.Exists(root)) Directory.Delete(root, true); } catch (IOException) { } catch (UnauthorizedAccessException) { }
        }
    }
}
