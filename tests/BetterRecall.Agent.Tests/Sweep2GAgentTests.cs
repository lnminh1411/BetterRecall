// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using BetterRecall.Agent.Worker;

namespace BetterRecall.Agent.Tests;

/// <summary>Bug sweep 2, batch G: agent side of the worker fixes (D34).</summary>
public class Sweep2GAgentTests
{
    [Fact]
    public async Task Cancelled_bench_stops_the_worker_without_a_backoff()
    {
        // The fake reads the bench request and never answers: a bench that runs for minutes.
        var launcher = new FakeWorkerLauncher { NoReply = req => req.Op == "bench" };
        await using var sup = new WorkerSupervisor(launcher, @"Z:\no-such-dir", () => 120);
        var client = new WorkerClient(sup);
        Assert.Equal(4, (await client.EmbedTextAsync("warm", "query", CancellationToken.None)).Length);

        using var cts = new CancellationTokenSource(TimeSpan.FromMilliseconds(300));
        await Assert.ThrowsAnyAsync<OperationCanceledException>(() => client.BenchAsync(cts.Token));
        Assert.Equal(1, launcher.Kills);
        Assert.Equal(0, sup.Failures);   // a cancelled request is not a crash: no backoff for the next start

        // The next request gets a fresh worker at once instead of queueing behind the bench.
        var v = await client.EmbedTextAsync("after", "query", CancellationToken.None).WaitAsync(TimeSpan.FromSeconds(20));
        Assert.Equal(5f, v[0]);
        Assert.Equal(2, launcher.Starts);
    }
}
