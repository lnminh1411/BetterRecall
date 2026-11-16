// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Diagnostics;
using System.Reflection;
using System.Text;
using System.Text.Json;
using BetterRecall.Contracts;
using BetterRecall.Core.Ai;
using BetterRecall.Core.Ipc;
using BetterRecall.Core.Store;
using Microsoft.Data.Sqlite;

namespace BetterRecall.Core.Tests;

/// <summary>Bug sweep 3, batch K (reliability), store items.</summary>
public class StoreCompactionSweep3Tests
{
    private static SqliteConnection MonthConn(EncryptedStore s, string month)
    {
        var kf = (Dictionary<string, SqliteConnection>)typeof(EncryptedStore).GetField("_kf", BindingFlags.NonPublic | BindingFlags.Instance)!.GetValue(s)!;
        return kf[month];
    }

    private static object? Scalar(SqliteConnection c, string sql)
    {
        using var cmd = c.CreateCommand();
        cmd.CommandText = sql;
        return cmd.ExecuteScalar();
    }

    /// <summary>B4: a legacy month file (auto_vacuum NONE) with many free pages is rebuilt by copying its keyframes into a
    /// new incremental-vacuum file, not VACUUMed (whose temporary copy lives in RAM). Kept keyframes read back the same.</summary>
    [Fact]
    public void Legacy_month_file_is_rebuilt_without_vacuum()
    {
        using var t = new TempStore();
        var now = DateTimeOffset.UtcNow;
        var month = now.UtcDateTime.ToString("yyyy-MM");
        var images = new List<(string Id, byte[] Bytes)>();
        for (int i = 0; i < 12; i++)
        {
            var img = new byte[200_000];
            new Random(i).NextBytes(img);
            var id = t.Store.AddScreenCapture(Caps.Screen(now.AddSeconds(-i - 1), image: img));
            t.Store.TakePendingEmbeddings(10);
            t.Store.SetEmbedding(id, Vec.Seeded(i), Vec.Info);   // ready: GetKeyframe serves ready rows only
            images.Add((id, img));
        }
        var c = MonthConn(t.Store, month);
        using (var cmd = c.CreateCommand()) { cmd.CommandText = "PRAGMA auto_vacuum=NONE; VACUUM;"; cmd.ExecuteNonQuery(); }
        Assert.Equal(0L, Scalar(c, "PRAGMA auto_vacuum"));
        for (int i = 0; i < 9; i++) Assert.Equal(1, t.Store.Purge(PurgeScope.ForId(images[i].Id)));
        t.Store.Checkpoint();
        var path = Path.Combine(DataPaths.KeyframesDir(t.Home.Path), month + ".db");
        long before = new FileInfo(path).Length;
        Assert.True((long)Scalar(c, "PRAGMA freelist_count")! > 0);

        t.Store.LegacyRebuildMinFreeBytes = 256 * 1024;
        var s = new Settings();
        s.Retention.Days = 3650;
        t.Store.RunMaintenance(s);

        long after = new FileInfo(path).Length;
        Assert.True(after < before / 2, $"{after} vs {before}");
        Assert.Empty(Directory.GetFiles(DataPaths.KeyframesDir(t.Home.Path), "*.tmp*"));
        for (int i = 9; i < 12; i++) Assert.Equal(images[i].Bytes, t.Store.GetKeyframe(images[i].Id)!.Bytes);
        Assert.Equal(2L, Scalar(MonthConn(t.Store, month), "PRAGMA auto_vacuum"));   // later deletes free pages at once
        t.Reopen();
        for (int i = 9; i < 12; i++) Assert.Equal(images[i].Bytes, t.Store.GetKeyframe(images[i].Id)!.Bytes);
    }

    /// <summary>A legacy month with many free pages: count images, all ready, then all but the last keep purged.</summary>
    private static (string Month, List<(string Id, byte[] Bytes)> Images) LegacyMonth(TempStore t, int count, int keep)
    {
        var now = DateTimeOffset.UtcNow;
        var month = now.UtcDateTime.ToString("yyyy-MM");
        var images = new List<(string Id, byte[] Bytes)>();
        for (int i = 0; i < count; i++)
        {
            var img = new byte[60_000];
            new Random(i).NextBytes(img);
            var id = t.Store.AddScreenCapture(Caps.Screen(now.AddSeconds(-count + i), image: img));
            t.Store.TakePendingEmbeddings(10);
            t.Store.SetEmbedding(id, Vec.Seeded(i), Vec.Info);
            images.Add((id, img));
        }
        var c = MonthConn(t.Store, month);
        using (var cmd = c.CreateCommand()) { cmd.CommandText = "PRAGMA auto_vacuum=NONE; VACUUM;"; cmd.ExecuteNonQuery(); }
        for (int i = 0; i < count - keep; i++) Assert.Equal(1, t.Store.Purge(PurgeScope.ForId(images[i].Id)));
        t.Store.Checkpoint();
        t.Store.LegacyRebuildMinFreeBytes = 256 * 1024;
        return (month, images.Skip(count - keep).ToList());
    }

    /// <summary>K review S3: the rebuild stops between batches while a priority caller (here a purge) waits for the
    /// writer lock, lets it in, then goes on. A keyframe the purge deleted after it was copied is gone from the new file.</summary>
    [Fact]
    public void Legacy_rebuild_pauses_for_a_purge_and_resumes()
    {
        using var t = new TempStore();
        var (month, kept) = LegacyMonth(t, 40, 10);
        var waiters = typeof(EncryptedStore).GetField("_priorityWaiters", BindingFlags.NonPublic | BindingFlags.Instance)!;
        t.Store.RebuildBatchRows = 2;
        Task<int>? purge = null;
        t.Store.AfterRebuildBatch = () =>
        {
            if (purge != null) return;
            purge = Task.Run(() => t.Store.Purge(PurgeScope.ForId(kept[0].Id)));   // copied in the first batch
            var sw = Stopwatch.StartNew();
            while ((int)waiters.GetValue(t.Store)! == 0 && sw.ElapsedMilliseconds < 5000) Thread.Sleep(1);
        };
        var s = new Settings();
        s.Retention.Days = 3650;
        t.Store.RunMaintenance(s);

        Assert.Equal(1, purge!.Result);
        Assert.True(t.Store.RebuildPauses >= 1);
        Assert.Null(t.Store.GetKeyframe(kept[0].Id));
        for (int i = 1; i < kept.Count; i++) Assert.Equal(kept[i].Bytes, t.Store.GetKeyframe(kept[i].Id)!.Bytes);
        Assert.Equal(2L, Scalar(MonthConn(t.Store, month), "PRAGMA auto_vacuum"));   // rebuilt
        Assert.Equal(9L, Scalar(MonthConn(t.Store, month), "SELECT count(*) FROM kf"));
        Assert.Empty(Directory.GetFiles(DataPaths.KeyframesDir(t.Home.Path), "*.tmp*"));
    }

    /// <summary>K review N3: a rebuild that throws something other than an IO or SQLite error (a non-blob value) drops
    /// only itself: the old file stays, no temporary file is left and the rest of the maintenance runs.</summary>
    [Fact]
    public void Failed_legacy_rebuild_leaves_the_month_and_the_maintenance_running()
    {
        using var t = new TempStore();
        var (month, kept) = LegacyMonth(t, 20, 5);
        var c = MonthConn(t.Store, month);
        using (var cmd = c.CreateCommand())
        {
            cmd.CommandText = "UPDATE kf SET data = 'not a blob' WHERE rid = (SELECT max(rid) FROM kf)";   // the last kept one
            cmd.ExecuteNonQuery();
        }
        var s = new Settings();
        s.Retention.Days = 3650;
        t.Store.RunMaintenance(s);   // threw InvalidCastException before

        foreach (var k in kept[..^1]) Assert.Equal(k.Bytes, t.Store.GetKeyframe(k.Id)!.Bytes);
        Assert.Equal(0L, Scalar(MonthConn(t.Store, month), "PRAGMA auto_vacuum"));   // not rebuilt, still the old file
        Assert.Empty(Directory.GetFiles(DataPaths.KeyframesDir(t.Home.Path), "*.tmp*"));
    }

    /// <summary>A store closed while a rebuild is paused deletes its temporary file (purge all removes every file).</summary>
    [Fact]
    public void Closing_the_store_drops_a_paused_rebuild()
    {
        using var t = new TempStore();
        var (_, _) = LegacyMonth(t, 20, 5);
        t.Store.RebuildBatchRows = 1;
        var release = typeof(EncryptedStore).GetMethod("ReleaseFreePagesLocked", BindingFlags.NonPublic | BindingFlags.Instance, [typeof(string), typeof(bool)])!;
        var waiters = typeof(EncryptedStore).GetField("_priorityWaiters", BindingFlags.NonPublic | BindingFlags.Instance)!;
        var wl = (Lock)typeof(EncryptedStore).GetField("_wl", BindingFlags.NonPublic | BindingFlags.Instance)!.GetValue(t.Store)!;
        var month = DateTimeOffset.UtcNow.UtcDateTime.ToString("yyyy-MM");
        waiters.SetValue(t.Store, 1);   // a priority caller is waiting: pause after the first batch
        bool paused;
        lock (wl) paused = (bool)release.Invoke(t.Store, [month, true])!;
        waiters.SetValue(t.Store, 0);
        Assert.True(paused);
        Assert.NotEmpty(Directory.GetFiles(DataPaths.KeyframesDir(t.Home.Path), "*.tmp*"));
        t.Store.Dispose();
        Assert.Empty(Directory.GetFiles(DataPaths.KeyframesDir(t.Home.Path), "*.tmp*"));
    }

    /// <summary>B4: store.db VACUUM (in RAM) runs only while the live size is within the RAM budget.</summary>
    [Fact]
    public void Store_db_vacuum_respects_the_ram_budget()
    {
        using var t = new TempStore();
        var old = DateTimeOffset.UtcNow.AddHours(-2);
        var rnd = new Random(1);
        var chunk = new byte[8000];
        for (int i = 0; i < 1600; i++)
        {
            rnd.NextBytes(chunk);
            t.Store.AddAudioCapture(Caps.Audio(old.AddSeconds(i), Convert.ToHexString(chunk)));
        }
        t.Store.Purge(PurgeScope.Range(old.AddSeconds(-1), old.AddSeconds(1500)));
        t.Store.Checkpoint();
        var db = DataPaths.StoreDb(t.Home.Path);
        long before = new FileInfo(db).Length;
        var s = new Settings();
        s.Retention.Days = 3650;
        t.Store.VacuumRamBudgetBytes = 0;
        t.Store.RunMaintenance(s);
        t.Store.Checkpoint();
        Assert.True(new FileInfo(db).Length >= before, "vacuumed over the budget");
        t.Store.VacuumRamBudgetBytes = 128L * 1024 * 1024;
        t.Store.RunMaintenance(s);
        t.Store.Checkpoint();
        Assert.True(new FileInfo(db).Length < before / 2, $"{new FileInfo(db).Length} vs {before}");
    }
}

public class ClaudeStreamSweep3Tests
{
    private static string Ev(string type, string json) => $"event: {type}\ndata: {json}\n\n";

    /// <summary>B10: text and thinking deltas build up in a StringBuilder and land in the block once. A 4 MiB answer
    /// used to take 8 s (each delta copied the whole string); the result is the same text.</summary>
    [Fact]
    public async Task Long_streams_parse_in_linear_time_with_the_same_content()
    {
        var sb = new StringBuilder();
        sb.Append(Ev("message_start", """{"type":"message_start","message":{"usage":{"input_tokens":1}}}"""));
        sb.Append(Ev("content_block_start", """{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"""));
        sb.Append(Ev("content_block_delta", """{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"hm "}}"""));
        sb.Append(Ev("content_block_delta", """{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"ok"}}"""));
        sb.Append(Ev("content_block_stop", """{"type":"content_block_stop","index":0}"""));
        sb.Append(Ev("content_block_start", """{"type":"content_block_start","index":1,"content_block":{"type":"text","text":"A"}}"""));
        var chunk = new string('x', 100);
        int deltas = 0;
        while (sb.Length < 4 << 20)
        {
            sb.Append(Ev("content_block_delta", "{\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"text_delta\",\"text\":\"" + chunk + "\"}}"));
            deltas++;
        }
        // no content_block_stop for block 1: the end of the stream still writes its text
        sb.Append(Ev("message_stop", """{"type":"message_stop"}"""));
        var body = new MemoryStream(Encoding.UTF8.GetBytes(sb.ToString()));
        var sw = Stopwatch.StartNew();
        var turn = await new ClaudeChat().ReadStreamAsync(body, _ => { }, CancellationToken.None);
        sw.Stop();
        Assert.True(sw.ElapsedMilliseconds < 4000, $"{sw.ElapsedMilliseconds} ms");
        var content = turn.Assistant["content"]!.AsArray();
        Assert.Equal("hm ok", content[0]!["thinking"]!.GetValue<string>());
        var text = content[1]!["text"]!.GetValue<string>();
        Assert.Equal(1 + deltas * 100, text.Length);
        Assert.StartsWith("Ax", text);
        Assert.Equal(deltas * 100, turn.Text.Length);
    }
}

public class PipeSettingsOrderSweep3Tests
{
    /// <summary>B13: two racing settings updates deliver Changed out of order (enabled=true held back behind
    /// enabled=false). PipeManager applies the newest settings, so the public pipe stays closed.</summary>
    [Fact]
    public async Task Stale_settings_snapshot_does_not_reopen_the_public_pipe()
    {
        using var t = new TempStore();
        using var home = new TempHome(null);
        var settings = SettingsStore.Load(home.Path);
        Assert.Null(settings.TryUpdate(JsonDocument.Parse("{\"ipc\":{\"enabled\":false}}").RootElement));
        var p = new CommandProcessor(new CommandDeps
        {
            Settings = settings, Store = () => t.Store, Keys = new FakeKeys(), Agent = new FakeAgent(),
            Embedder = new FakeEmbedder(), Thumbnailer = new FakeThumbs(), PurgeAll = _ => Task.FromResult(0),
        });
        var tag = Guid.NewGuid().ToString("N")[..10];
        var pipes = new PipeManager(settings, p, @"C:\NotTheInstallDir", () => t.Store, (k, s) => $"BrSweep3K.{tag}.{k}");
        var bDelivered = new ManualResetEventSlim();
        settings.Changed += s => { if (s.Ipc.Enabled) bDelivered.Wait(5000); };   // before PipeManager: holds back A
        await pipes.StartAsync();
        settings.Changed += s => { if (!s.Ipc.Enabled) bDelivered.Set(); };
        try
        {
            var t1 = Task.Run(() => settings.TryUpdate(JsonDocument.Parse("{\"ipc\":{\"enabled\":true}}").RootElement));
            await Task.Delay(300);
            Assert.Null(settings.TryUpdate(JsonDocument.Parse("{\"ipc\":{\"enabled\":false}}").RootElement));
            Assert.Null(await t1);
            await Task.Delay(500);
            Assert.False(settings.Current.Ipc.Enabled);
            Assert.Null(pipes.Public);
        }
        finally { await pipes.DisposeAsync(); }
    }
}

public class SettingsLoadSweep3Tests
{
    /// <summary>D3: one unescaped backslash used to reset every setting and overwrite the file with defaults. The file
    /// now stays as it was until the user saves; LoadError feeds the settings-unreadable status warning.</summary>
    [Fact]
    public void Unparsable_file_is_kept_until_the_next_save()
    {
        using var h = new TempHome(null);
        var path = DataPaths.SettingsFile(h.Path);
        const string bad = "{\"capture\":{\"consented\":true},\"inference\":{\"modelDir\":\"C:\\models\"}}";   // \m: bad escape
        File.WriteAllText(path, bad);
        var s = SettingsStore.Load(h.Path);
        Assert.False(s.Current.Capture.Consented);   // defaults in memory
        Assert.NotNull(s.LoadError);
        Assert.Equal(bad, File.ReadAllText(path));   // not overwritten
        Assert.Equal(bad, File.ReadAllText(path + ".bad"));
        Assert.Null(s.TryUpdate(JsonDocument.Parse("{\"retention\":{\"days\":60}}").RootElement));
        Assert.Null(s.LoadError);                    // the user saved: the file is the settings again
        Assert.Equal(60, SettingsStore.ReadOnly(h.Path).Retention.Days);
    }

    [Fact]
    public void Readable_and_missing_files_have_no_load_error()
    {
        using var h = new TempHome(null);
        Assert.Null(SettingsStore.Load(h.Path).LoadError);   // missing: written with defaults
        Assert.Null(SettingsStore.Load(h.Path).LoadError);   // readable
    }
}

public sealed class SearchSweep3Tests : IDisposable
{
    /// <summary>A worker that is not running: answers only after a long start, honouring cancellation.</summary>
    private sealed class ColdEmbedder : IQueryEmbedder
    {
        public bool IsWarm;
        public bool Throw;
        public async Task<float[]> EmbedQueryAsync(string text, CancellationToken ct)
        {
            if (Throw) throw new InvalidOperationException("worker down");
            await Task.Delay(TimeSpan.FromSeconds(30), ct);
            return Vec.Seeded(1);
        }
        public bool Warm => IsWarm;
    }

    private readonly TempStore _t = new();
    private readonly TempHome _home = new(null);
    private readonly ColdEmbedder _embed = new();
    private readonly CommandProcessor _p;
    private static readonly ClientSession Ui = new(PipeKind.Ui, 101, @"C:\app\ui\BetterRecall.UI.exe");

    public SearchSweep3Tests()
    {
        var settings = SettingsStore.Load(_home.Path);
        _p = new CommandProcessor(new CommandDeps
        {
            Settings = settings, Store = () => _t.Store, Keys = new FakeKeys(), Agent = new FakeAgent(),
            Embedder = _embed, Thumbnailer = new FakeThumbs(), PurgeAll = _ => Task.FromResult(0),
        });
        var id = _t.Store.AddScreenCapture(Caps.Screen(DateTimeOffset.UtcNow.AddMinutes(-5), "chrome.exe", "Acme invoice 4471 - Gmail"));
        _t.Store.SetEmbedding(id, Vec.Seeded(1), Vec.Info);
    }

    public void Dispose()
    {
        _t.Dispose();
        _home.Dispose();
    }

    private async Task<SearchResponse> Search(string q)
    {
        var r = await _p.HandleAsync(Ui, "search " + q, CancellationToken.None);
        var (ok, data, err) = Envelope.Parse(r.Line);
        Assert.True(ok, err);
        return data.Deserialize(ContractsJson.Default.SearchResponse)!;
    }

    /// <summary>D5: with the worker not running the first search waited 15 s for the embed, then answered text-only
    /// without saying so. Now it waits briefly, answers from text, and flags degraded "vector".</summary>
    [Fact]
    public async Task Cold_worker_gives_text_results_quickly_and_flags_them()
    {
        var sw = System.Diagnostics.Stopwatch.StartNew();
        var resp = await Search("acme");
        Assert.True(sw.ElapsedMilliseconds < 10_000, $"{sw.ElapsedMilliseconds} ms");
        Assert.Equal("vector", resp.Degraded);
        Assert.Single(resp.Results);
        Assert.True(resp.Results[0].TextMatch);
    }

    /// <summary>K review S2: a failed embedding (model not installed, VC++ runtime missing, restart backoff) is not
    /// "starting, try again": it gets its own value, warm or cold.</summary>
    [Fact]
    public async Task Failed_embedding_is_flagged_and_a_good_one_is_not()
    {
        _embed.Throw = true;
        Assert.Equal("vector-unavailable", (await Search("acme")).Degraded);   // cold, but it failed: not a timeout
        _embed.IsWarm = true;
        Assert.Equal("vector-unavailable", (await Search("acme")).Degraded);
        var json = (await _p.HandleAsync(Ui, "search {\"q\":\"acme\",\"mode\":\"text\"}", CancellationToken.None)).Line;
        Assert.DoesNotContain("degraded", json);   // text mode never asked for a vector
    }

    /// <summary>D11: with a query vector but no text hit, the score is the cosine, not the rank-based 0.5 that made
    /// nonsense queries look like confident matches.</summary>
    [Fact]
    public void Hybrid_search_without_text_hits_reports_the_cosine()
    {
        var q = Vec.Seeded(2);
        var resp = _t.Store.Search(new SearchRequest("zzqqxx"), q, 10, 0);
        var hit = Assert.Single(resp.Results);
        Assert.False(hit.TextMatch);
        Assert.Equal(Math.Round(Math.Max(0, hit.Cosine!.Value), 4), hit.Score, 4);
        Assert.NotEqual(0.5f, hit.Score);
        // With a text hit the fused rank score stays.
        var fused = _t.Store.Search(new SearchRequest("acme"), q, 10, 0);
        Assert.True(Assert.Single(fused.Results).TextMatch);
    }
}
