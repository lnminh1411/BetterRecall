// SPDX-License-Identifier: MIT
// Copyright (c) BetterRecall contributors
using System.Diagnostics;
using BetterRecall.Contracts;
using BetterRecall.UI.Logic;

namespace BetterRecall.UI.Tests;

/// <summary>Sweep 5 batch L (UI client side): StepClock multi-step monotonic _until (C3) and junction RealPath trust (B8).</summary>
public class Sweep5LUiTests
{
    [Fact]
    public void C3_StepClock_preserves_until_across_multiple_backward_steps_and_sub_tolerance_steps()
    {
        var steps = new List<TimeSpan>();
        var clock = new StepClock(steps.Add);
        var boot = new DateTime(2026, 11, 18, 12, 0, 0, DateTimeKind.Utc);
        long ms = 60_000;
        var wall = boot.AddMilliseconds(ms);   // 12:01
        Assert.Equal(wall, clock.Bound(wall, ms, boot));

        ms += 240_000;
        wall = new DateTime(2026, 11, 18, 11, 5, 0, DateTimeKind.Utc);
        Assert.Equal(new DateTime(2026, 11, 18, 12, 5, 0, DateTimeKind.Utc), clock.Bound(wall, ms, boot));

        // Second step back (-2 min) must keep _until = 12:05 rather than shrinking it to 11:10.
        ms += 300_000;
        wall = new DateTime(2026, 11, 18, 11, 8, 0, DateTimeKind.Utc);
        Assert.Equal(new DateTime(2026, 11, 18, 12, 5, 0, DateTimeKind.Utc), clock.Bound(wall, ms, boot));
        Assert.Equal(2, steps.Count);

        // Multiple sub-10 s backward steps (-6 s x 3) accumulate until StepTolerance is exceeded.
        var subSteps = new List<TimeSpan>();
        var subClock = new StepClock(subSteps.Add);
        var subWall = new DateTime(2026, 11, 18, 12, 1, 0, DateTimeKind.Utc);
        Assert.Equal(subWall, subClock.Bound(subWall, 60_000, boot));
        Assert.Equal(subWall.AddSeconds(-6), subClock.Bound(subWall.AddSeconds(-6), 60_000, boot));
        Assert.Empty(subSteps);
        Assert.Equal(subWall, subClock.Bound(subWall.AddSeconds(-12), 60_000, boot));
        Assert.Single(subSteps);
    }

    [Fact]
    public async Task C3_AgentClient_stays_trusted_across_two_backward_clock_steps()
    {
        await using var server = new FakeLineServer(_ => Envelope.Ok("null"));
        var clock = new StepClock();
        long ms = Environment.TickCount64;
        var wall = DateTime.UtcNow;
        var boot = wall.AddMilliseconds(-ms);
        clock.Bound(wall, ms, boot);
        var created = wall.AddMinutes(2);   // server started 2 min later, before the -60 min step

        DateTime NextBound() => clock.Bound(wall, ms, boot);
        var verifier = new ServerTrustVerifier(Environment.ProcessPath!, startedOf: _ => created, startBound: NextBound);

        ms += 240_000;
        wall = wall.AddMinutes(-56);   // +4 min elapsed, -60 min clock step -> _until = T0 + 4m
        await using (var c1 = new AgentClient(server.Name, verifier))
            Assert.True((await c1.SendAsync("ping")).Ok);

        // Second step back (-2 min) 5 min later: fresh connection runs Verify again and _until must stay T0 + 4m.
        ms += 300_000;
        wall = wall.AddMinutes(3);
        await using (var c2 = new AgentClient(server.Name, verifier))
            Assert.True((await c2.SendAsync("ping")).Ok);
    }

    [Fact]
    public async Task B8_ServerTrustVerifier_resolves_junction_expected_image_against_real_process_image()
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
            p.WaitForExit(5000);
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
