// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using BetterRecall.Agent.Capture;

namespace BetterRecall.Agent.Tests;

public class EncodeGovernorTests
{
    private sealed class FakeClock
    {
        public long NowMs;
    }

    private sealed class Item(int id, long offeredAt = 0) : IDisposable
    {
        public int Id { get; } = id;
        public long OfferedAt { get; set; } = offeredAt;
        public int DisposeCount { get; private set; }

        public void Dispose()
        {
            DisposeCount++;
        }
    }

    private sealed class Harness
    {
        public FakeClock Clock { get; } = new();
        public List<(long TimeMs, EncodeLevel Level, double CostMs, int ItemId)> Encodes { get; } = [];
        public List<(int ItemId, string Reason)> Drops { get; } = [];
        public double[] Costs { get; }
        public EncodeGovernor<Item> Governor { get; }

        public Harness(double[] costs, double[]? initialEstimates = null, int procs = 20)
        {
            Costs = costs;
            double[] estimates = initialEstimates ?? (double[])costs.Clone();

            Governor = new EncodeGovernor<Item>(
                () => Clock.NowMs,
                procs,
                (item, level) =>
                {
                    double cost = Costs[(int)level];
                    Encodes.Add((Clock.NowMs, level, cost, item.Id));
                    return cost;
                },
                (item, reason) =>
                {
                    Drops.Add((item.Id, reason));
                },
                estimates
            );
        }
    }

    private static void AssertWithinBudget(IReadOnlyList<(long TimeMs, EncodeLevel Level, double CostMs, int ItemId)> encodes, double limit10 = 3000.0, double limit60 = 12000.0)
    {
        foreach (var (x, _, _, _) in encodes)
        {
            double sum10 = encodes.Where(e => e.TimeMs > x - 10_000 && e.TimeMs <= x).Sum(e => e.CostMs);
            double sum60 = encodes.Where(e => e.TimeMs > x - 60_000 && e.TimeMs <= x).Sum(e => e.CostMs);
            Assert.True(sum10 <= limit10 + 1e-6, $"10s budget exceeded at t={x}: sum={sum10} > limit={limit10}");
            Assert.True(sum60 <= limit60 + 1e-6, $"60s budget exceeded at t={x}: sum={sum60} > limit={limit60}");
        }
    }

    [Fact]
    public void Limits_calculate_correctly_from_processor_count()
    {
        var gov20 = new EncodeGovernor<Item>(() => 0, 20, (_, _) => 0, (_, _) => { }, [10, 10, 10, 10]);
        Assert.Equal(3000, gov20.Limit10Ms);
        Assert.Equal(12000, gov20.Limit60Ms);

        var gov8 = new EncodeGovernor<Item>(() => 0, 8, (_, _) => 0, (_, _) => { }, [10, 10, 10, 10]);
        Assert.Equal(1200, gov8.Limit10Ms);
        Assert.Equal(4800, gov8.Limit60Ms);
    }

    [Fact]
    public void Light_load_stays_at_normal()
    {
        var harness = new Harness([100, 60, 30, 20]);
        int offered = 0;
        // 10 simulated minutes = 600 s. One item every 2 s = 300 items.
        for (long t = 2000; t <= 600_000; t += 2000)
        {
            harness.Clock.NowMs = t;
            harness.Governor.Offer(new Item(++offered, t));
        }

        Assert.Equal(300, harness.Encodes.Count);
        Assert.All(harness.Encodes, e => Assert.Equal(EncodeLevel.Normal, e.Level));
        Assert.Equal(0, harness.Governor.Deferred);
        Assert.Equal(0, harness.Governor.Dropped);
        Assert.Equal(0, harness.Governor.Waiting);
        AssertWithinBudget(harness.Encodes);
    }

    [Fact]
    public void Heavy_load_steps_down_and_holds_the_budget()
    {
        var harness = new Harness([300, 150, 60, 30]);
        int offered = 0;

        for (long t = 2000; t <= 600_000; t += 2000)
        {
            harness.Clock.NowMs = t;
            for (int i = 0; i < 3; i++)
            {
                harness.Governor.Offer(new Item(++offered, t));
            }

            if (t % 30_000 == 0)
            {
                for (int i = 0; i < 6; i++)
                {
                    harness.Governor.Offer(new Item(++offered, t));
                }
            }

            harness.Governor.Pump();
        }

        AssertWithinBudget(harness.Encodes);
        Assert.True(harness.Governor.Downgraded > 0, "Downgraded counter should be > 0");
        Assert.Contains(harness.Encodes, e => e.Level == EncodeLevel.Fast);
        Assert.Contains(harness.Encodes, e => e.Level == EncodeLevel.Reduced);
        Assert.Equal(offered, harness.Governor.Encoded + harness.Governor.Dropped + harness.Governor.Waiting);

        double totalCost = harness.Encodes.Sum(e => e.CostMs);
        double avgShare = totalCost / (600_000.0 * 20);
        Assert.True(avgShare < 0.01, $"Average CPU share {avgShare:P2} should be < 1%");
    }

    [Fact]
    public void Newest_is_never_dropped()
    {
        var harness = new Harness([800, 600, 500, 400]);
        int offered = 0;
        int lastOfferedId = 0;

        var dropViolations = new List<(int DroppedId, int LastOfferedId)>();
        var gov = new EncodeGovernor<Item>(
            () => harness.Clock.NowMs,
            20,
            (item, level) =>
            {
                double cost = harness.Costs[(int)level];
                harness.Encodes.Add((harness.Clock.NowMs, level, cost, item.Id));
                return cost;
            },
            (item, reason) =>
            {
                if (item.Id == lastOfferedId)
                    dropViolations.Add((item.Id, lastOfferedId));
                harness.Drops.Add((item.Id, reason));
            },
            harness.Costs
        );

        for (long t = 2000; t <= 600_000; t += 2000)
        {
            harness.Clock.NowMs = t;
            for (int i = 0; i < 3; i++)
            {
                lastOfferedId = ++offered;
                gov.Offer(new Item(offered, t));
            }

            if (t % 30_000 == 0)
            {
                for (int i = 0; i < 6; i++)
                {
                    lastOfferedId = ++offered;
                    gov.Offer(new Item(offered, t));
                }
            }

            gov.Pump();
        }

        Assert.True(harness.Drops.Count > 0, "Expected drops under heavy load");
        Assert.Empty(dropViolations);
    }

    [Fact]
    public void Burst_then_catch_up()
    {
        var harness = new Harness([200, 100, 50, 25]);
        harness.Clock.NowMs = 0;

        // 40 at once: the window takes about 6 Normal, 6 Fast and 12 Reduced, the rest wait or overflow the queue.
        for (int i = 1; i <= 40; i++)
        {
            harness.Governor.Offer(new Item(i, 0));
        }
        Assert.Contains(harness.Encodes, e => e.Level == EncodeLevel.Reduced);

        // Right after burst:
        double sumAt0 = harness.Encodes.Where(e => e.TimeMs == 0).Sum(e => e.CostMs);
        Assert.True(sumAt0 <= 0.8 * harness.Governor.Limit10Ms, $"Initial encodes sum={sumAt0} exceeds 10s target");
        Assert.True(harness.Governor.Waiting > 0 || harness.Governor.WebPFallbacks > 0 || harness.Governor.Dropped > 0);

        // Advance clock in 1 s steps to 120 s calling Pump() each step
        for (long t = 1000; t <= 120_000; t += 1000)
        {
            harness.Clock.NowMs = t;
            harness.Governor.Pump();
        }

        Assert.Equal(0, harness.Governor.Waiting);
        AssertWithinBudget(harness.Encodes);
        Assert.Equal(40, harness.Governor.Encoded + harness.Governor.Dropped);
    }

    [Fact]
    public void Stale_frames_go_to_webp()
    {
        // Estimates set so Normal, Fast, and Reduced never fit the 10 s window target (0.8 * 3000 = 2400)
        // while WebP (cost 10) fits easily. The frame cannot be chosen immediately, so it waits.
        // Once clock advances past MaxWaitMs (30,000 ms), Pump() considers it stale and falls back to WebP.
        var harness = new Harness([5000, 5000, 5000, 10], initialEstimates: [5000, 5000, 5000, 10]);
        harness.Clock.NowMs = 0;

        var item = new Item(1, 0);
        harness.Governor.Offer(item);
        Assert.Equal(1, harness.Governor.Waiting);
        Assert.Equal(0, harness.Governor.Encoded);

        // Advance clock past MaxWaitMs (30,000 ms)
        harness.Clock.NowMs = EncodeGovernor<Item>.MaxWaitMs + 1000;
        harness.Governor.Pump();

        Assert.Equal(0, harness.Governor.Waiting);
        Assert.Equal(1, harness.Governor.Encoded);
        Assert.Equal(1, harness.Governor.WebPFallbacks);
        Assert.Single(harness.Encodes);
        Assert.Equal(EncodeLevel.WebP, harness.Encodes[0].Level);
    }

    [Fact]
    public void Estimates_follow_measured_costs()
    {
        // Initial estimates 1000, real cost 100. Each encode moves estimate by e = e * 0.7 + ms * 0.3.
        var harness = new Harness([100, 100, 100, 100], initialEstimates: [1000, 1000, 1000, 1000]);
        harness.Clock.NowMs = 0;

        for (int i = 1; i <= 10; i++)
        {
            // Spread over time by 15 s so window limits never block Normal
            harness.Clock.NowMs += 15_000;
            harness.Governor.Offer(new Item(i, harness.Clock.NowMs));
        }

        Assert.Equal(10, harness.Governor.Encoded);
        Assert.True(harness.Governor.Estimate(EncodeLevel.Normal) < 200,
            $"Estimate {harness.Governor.Estimate(EncodeLevel.Normal)} should be below 200 after 10 encodes");
    }

    [Fact]
    public void Every_item_is_disposed_exactly_once()
    {
        var harness = new Harness([300, 150, 60, 30]);
        var allItems = new List<Item>();
        int id = 0;

        for (long t = 2000; t <= 600_000; t += 2000)
        {
            harness.Clock.NowMs = t;
            for (int i = 0; i < 3; i++)
            {
                var item = new Item(++id, t);
                allItems.Add(item);
                harness.Governor.Offer(item);
            }

            if (t % 30_000 == 0)
            {
                for (int i = 0; i < 6; i++)
                {
                    var item = new Item(++id, t);
                    allItems.Add(item);
                    harness.Governor.Offer(item);
                }
            }

            harness.Governor.Pump();
        }

        harness.Governor.Clear();

        Assert.NotEmpty(allItems);
        Assert.All(allItems, item => Assert.Equal(1, item.DisposeCount));
    }
}
