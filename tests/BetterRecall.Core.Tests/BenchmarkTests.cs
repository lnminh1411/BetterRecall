// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Diagnostics;
using BetterRecall.Contracts;
using Xunit.Abstractions;

namespace BetterRecall.Core.Tests;

public class BenchmarkTests(ITestOutputHelper output)
{
    /// <summary>
    /// 250k synthetic 768-d rows in the encrypted store. Measures the store part of a search (binary scan,
    /// float rerank from SQLite, FTS5, fusion, metadata). Target: under 100 ms.
    /// </summary>
    [Fact]
    public void Search_latency_at_250k_rows()
    {
        const int rows = 250_000, dim = 768;
        using var t = new TempStore();
        var rnd = new Random(12345);
        var keep = new List<float[]>();
        var now = DateTimeOffset.UtcNow;
        string[] apps = ["chrome.exe", "code.exe", "teams.exe", "explorer.exe", "excel.exe", "outlook.exe"];
        string[] words = ["invoice", "report", "meeting", "budget", "design", "review", "photo", "ticket", "release", "draft", "roadmap", "email"];

        IEnumerable<(DateTimeOffset, string, string, float[])> Gen()
        {
            for (int i = 0; i < rows; i++)
            {
                var v = new float[dim];
                for (int j = 0; j < dim; j++) v[j] = (float)(rnd.NextDouble() * 2 - 1);
                if (i % 5000 == 0) keep.Add(v);
                yield return (now.AddSeconds(-i * 10), apps[i % apps.Length], $"{words[i % words.Length]} {words[(i / 7) % words.Length]} window {i}", v);
            }
        }

        var sw = Stopwatch.StartNew();
        t.Store.InsertSynthetic(Gen(), new EmbeddingInfo("bench", "r1", dim));
        var insert = sw.Elapsed;
        sw.Restart();
        t.Reopen();
        var load = sw.Elapsed;
        Assert.Equal(rows, t.Store.Index.Count);
        long dbBytes = new FileInfo(DataPaths.StoreDb(t.Home.Path)).Length + new FileInfo(DataPaths.StoreDb(t.Home.Path) + "-wal").Length;

        var queries = keep.Select((v, i) => (Q: v.Select(x => x + (float)(rnd.NextDouble() - 0.5) * 0.2f).ToArray(), Word: words[i % words.Length])).ToList();
        // warm-up
        foreach (var (q, w) in queries.Take(3)) t.Store.Search(new SearchRequest(w), q, 10, 262144);

        double Measure(SearchMode mode, Func<int, SearchRequest> req, out int hitsOk)
        {
            var times = new List<double>();
            hitsOk = 0;
            for (int i = 0; i < queries.Count; i++)
            {
                var s2 = Stopwatch.StartNew();
                var r = t.Store.Search(req(i) with { Mode = mode }, queries[i].Q, 10, 262144);
                times.Add(s2.Elapsed.TotalMilliseconds);
                if (mode != SearchMode.Text && r.Count > 0 && r.Results[0].Title.EndsWith(" window " + (i * 5000))) hitsOk++;
            }
            output.WriteLine($"  last: {t.Store.LastTimings} | {t.Store.LastVectorTimings}");
            times.Sort();
            var median = times[times.Count / 2];
            output.WriteLine($"{mode,-7} median {median:F1} ms, p90 {times[(int)(times.Count * 0.9)]:F1} ms, max {times[^1]:F1} ms, top-1 recall {hitsOk}/{queries.Count}");
            return median;
        }

        {
            var ts = new List<double>();
            foreach (var (q, _) in queries)
            {
                var s3 = Stopwatch.StartNew();
                t.Store.Index.Scan(q, BetterRecall.Core.Store.VectorFilter.All, 400);
                ts.Add(s3.Elapsed.TotalMilliseconds);
            }
            ts.Sort();
            output.WriteLine($"binary scan only: median {ts[ts.Count / 2]:F1} ms");
        }
        output.WriteLine($"rows {rows} x {dim}-d, insert {insert.TotalSeconds:F1} s, reopen + index load {load.TotalMilliseconds:F0} ms, store.db {dbBytes / 1048576.0:F0} MiB");
        var vec = Measure(SearchMode.Vector, i => new SearchRequest("x", 10), out var vecHits);
        var hyb = Measure(SearchMode.Hybrid, i => new SearchRequest(queries[i].Word + " window", 10), out _);
        var txt = Measure(SearchMode.Text, i => new SearchRequest(queries[i].Word, 10), out _);
        var ranged = Measure(SearchMode.Hybrid, i => new SearchRequest(queries[i].Word, 10, now.AddDays(-7), now, "chrome.exe"), out _);
        Assert.True(vecHits >= queries.Count * 9 / 10, $"vector recall {vecHits}/{queries.Count}");
        Assert.True(vec < 100, $"vector median {vec:F1} ms");
        Assert.True(hyb < 100, $"hybrid median {hyb:F1} ms");
        Assert.True(txt < 100, $"text median {txt:F1} ms");
        Assert.True(ranged < 100, $"filtered median {ranged:F1} ms");
    }
}
