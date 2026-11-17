// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using BetterRecall.Contracts;

namespace BetterRecall.Core.Tests;

/// <summary>Bug sweep 4, batch W: store halves of A-M1 (job leases expire) and C5 (going back up in dim). The agent
/// seams are in Agent.Tests Sweep4WAgentTests.</summary>
public class Sweep4WCoreTests
{
    private static readonly DateTimeOffset T0 = DateTimeOffset.UtcNow.AddMinutes(-30);

    /// <summary>A-M1 (rewrite of sweeper A's probe): a lease still holds a job back from the next take, as before, but
    /// only for EmbeddingLeaseTimeout. Jobs a client dropped without a reply (the agent's panic) come back after it.</summary>
    [Fact]
    public void Embedding_leases_hold_jobs_back_until_they_expire()
    {
        using var t = new TempStore();
        Assert.Equal(TimeSpan.FromMinutes(2), t.Store.EmbeddingLeaseTimeout);
        for (int i = 0; i < 3; i++) t.Store.AddScreenCapture(Caps.Screen(T0.AddSeconds(i), title: $"row {i}"));
        Assert.Equal(3, t.Store.TakePendingEmbeddings(32).Count);
        Assert.Empty(t.Store.TakePendingEmbeddings(32));            // leased
        t.Store.EmbeddingLeaseTimeout = TimeSpan.Zero;               // the leases are past their time
        var again = t.Store.TakePendingEmbeddings(32);
        Assert.Equal(3, again.Count);
        t.Store.EmbeddingLeaseTimeout = TimeSpan.FromMinutes(2);
        Assert.Empty(t.Store.TakePendingEmbeddings(32));            // the new take leased them again
        foreach (var j in again) t.Store.SetEmbedding(j.Id, Vec.Seeded(j.Id.Length), Vec.Info);
        Assert.Equal(0, t.Store.Stats().PendingEmbeddings);
    }

    /// <summary>C5: index.dim lowered (vectors truncated in place) and raised again with the same model. The truncated
    /// rows used to stay short for good; rows whose keyframe is kept get a job again.</summary>
    [Fact]
    public void Going_back_up_in_dim_queues_the_truncated_rows()
    {
        using var t = new TempStore();
        t.Store.EnsureEmbeddingSpace(Vec.Info);
        var kept = t.Store.AddScreenCapture(Caps.Screen(T0, title: "kept"));
        var dropped = t.Store.AddScreenCapture(Caps.Screen(T0.AddSeconds(1), title: "no keyframe kept", keep: false));
        t.Store.TakePendingEmbeddings(8);
        t.Store.SetEmbedding(kept, Vec.Seeded(1), Vec.Info);
        t.Store.SetEmbedding(dropped, Vec.Seeded(2), Vec.Info);   // its keyframe is deleted once embedded
        t.Store.EnsureEmbeddingSpace(Vec.Info with { Dim = 512 });
        Assert.Equal(0, t.Store.Stats().PendingEmbeddings);
        t.Store.EnsureEmbeddingSpace(Vec.Info);
        Assert.Equal(1, t.Store.Stats().PendingEmbeddings);   // only the row that can be embedded again
        var job = t.Store.TakePendingEmbeddings(8).Single();
        Assert.Equal(kept, job.Id);
        t.Store.SetEmbedding(kept, Vec.Seeded(1), Vec.Info);
        Assert.Equal(0, t.Store.Stats().PendingEmbeddings);
    }
}
