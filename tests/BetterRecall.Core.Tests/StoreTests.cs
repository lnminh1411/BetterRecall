// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Text;
using BetterRecall.Contracts;
using BetterRecall.Core.Store;
using Microsoft.Data.Sqlite;

namespace BetterRecall.Core.Tests;

public class StoreTests
{
    private static readonly DateTimeOffset T0 = DateTimeOffset.UtcNow.AddHours(-2);

    private static string AddReady(EncryptedStore s, DateTimeOffset ts, string title, float[] v, string app = "chrome.exe", bool keep = true)
    {
        var id = s.AddScreenCapture(Caps.Screen(ts, app, title, keep));
        var job = s.TakePendingEmbeddings(10).Single(j => j.Id == id);
        Assert.NotNull(job.Image);
        s.SetEmbedding(id, v, Vec.Info);
        return id;
    }

    internal static byte[] Raw(string path)
    {
        using var fs = new FileStream(path, FileMode.Open, FileAccess.Read, FileShare.ReadWrite | FileShare.Delete);
        using var ms = new MemoryStream();
        fs.CopyTo(ms);
        return ms.ToArray();
    }

    private static SearchResponse Find(EncryptedStore s, string q, float[]? qv = null, SearchMode mode = SearchMode.Hybrid, int max = 50, int bytes = 1 << 20)
        => s.Search(new SearchRequest(q, max, Mode: mode), qv ?? [], max, bytes);

    [Fact]
    public void Files_are_encrypted_not_plaintext_sqlite()
    {
        using var t = new TempStore();
        AddReady(t.Store, T0, "Acme invoice 4471 - Gmail", Vec.Seeded(1));
        t.Store.Checkpoint();
        var db = Raw(DataPaths.StoreDb(t.Home.Path));
        Assert.False(Encoding.ASCII.GetString(db, 0, 16).StartsWith("SQLite format 3"));
        Assert.DoesNotContain("Acme invoice", Encoding.UTF8.GetString(db));
        var kf = Directory.GetFiles(DataPaths.KeyframesDir(t.Home.Path), "*.db").Single();
        var kfBytes = Raw(kf);
        Assert.False(Encoding.ASCII.GetString(kfBytes, 0, 16).StartsWith("SQLite format 3"));
        Assert.DoesNotContain("MARKER-PIXELS", Encoding.ASCII.GetString(kfBytes));
    }

    [Fact]
    public void Wrong_key_cannot_open()
    {
        using var t = new TempStore();
        AddReady(t.Store, T0, "x", Vec.Seeded(1));
        t.Store.Dispose();
        var ex = Assert.Throws<SqliteException>(() => new EncryptedStore(t.Home.Path, () => new byte[32]));
        Assert.Equal(26, ex.SqliteErrorCode); // SQLITE_NOTADB
        t.Reopen();
    }

    /// <summary>Research section 8 spike: FTS5 and float/binary vector BLOBs under SQLite3MC, key set first, survive reopen.</summary>
    [Fact]
    public void Spike_fts5_and_vector_blobs_under_sqlite3mc_survive_reopen()
    {
        using var t = new TempStore();
        var v = Vec.Seeded(42);
        var id = AddReady(t.Store, T0, "Quarterly report draft", v);
        t.Reopen();
        var text = Find(t.Store, "quarterly", mode: SearchMode.Text);
        Assert.Equal(id, text.Results.Single().Id);
        var vec = Find(t.Store, "anything", v, SearchMode.Vector);
        Assert.Equal(id, vec.Results[0].Id);
        Assert.Equal(1, t.Store.Index.Count);
        Assert.Equal(1, t.Store.Stats().Vectors);
    }

    [Fact]
    public void Pending_rows_are_invisible_until_embedded()
    {
        using var t = new TempStore();
        var id = t.Store.AddScreenCapture(Caps.Screen(T0, title: "Secret pending title"));
        Assert.Empty(Find(t.Store, "secret").Results);
        Assert.Null(t.Store.Get(id));
        Assert.Null(t.Store.GetKeyframe(id));
        Assert.Empty(t.Store.Timeline(T0.AddDays(-1), T0.AddDays(1), 10));
        Assert.Equal(1, t.Store.Stats().PendingEmbeddings);
        t.Store.SetEmbedding(id, Vec.Seeded(3), Vec.Info);
        Assert.Single(Find(t.Store, "secret").Results);
        Assert.NotNull(t.Store.Get(id));
        var kf = t.Store.GetKeyframe(id)!;
        Assert.Equal(Caps.FakeWebp, kf.Bytes);
        Assert.Equal("image/webp", kf.Mime);
        Assert.Single(t.Store.Timeline(T0.AddDays(-1), T0.AddDays(1), 10));
    }

    [Fact]
    public void Take_does_not_hand_out_the_same_job_twice()
    {
        using var t = new TempStore();
        for (int i = 0; i < 5; i++) t.Store.AddScreenCapture(Caps.Screen(T0.AddSeconds(i)));
        var a = t.Store.TakePendingEmbeddings(3);
        var b = t.Store.TakePendingEmbeddings(3);
        Assert.Equal(3, a.Count);
        Assert.Equal(2, b.Count);
        Assert.Empty(a.Select(j => j.Id).Intersect(b.Select(j => j.Id)));
    }

    [Fact]
    public void Nsfw_drop_leaves_no_trace()
    {
        using var t = new TempStore();
        var id = t.Store.AddScreenCapture(Caps.Screen(T0, title: "Dropped frame title", image: Encoding.ASCII.GetBytes("DROPPED-IMAGE-BYTES-xxxxxxxx")));
        t.Store.TakePendingEmbeddings(5);
        t.Store.DropCapture(id, "nsfw");
        t.Store.Checkpoint();
        Assert.Null(t.Store.Get(id));
        Assert.Null(t.Store.GetKeyframe(id));
        Assert.Empty(Find(t.Store, "dropped").Results);
        Assert.Empty(t.Store.TakePendingEmbeddings(5));
        var st = t.Store.Stats();
        Assert.Equal(0, st.Rows);
        Assert.Equal(0, st.PendingEmbeddings);
        Assert.Equal(0, st.Vectors);
        // A late SetEmbedding for the dropped id must not resurrect it.
        t.Store.SetEmbedding(id, Vec.Seeded(1), Vec.Info);
        Assert.Null(t.Store.Get(id));
        Assert.Equal(0, t.Store.Index.Count);
        Assert.Equal(0, t.Store.CountPurge(new PurgeScope("all")));
    }

    [Fact]
    public void Failed_embedding_stays_invisible()
    {
        using var t = new TempStore();
        var id = t.Store.AddScreenCapture(Caps.Screen(T0, title: "Failure case"));
        t.Store.FailEmbedding(id, "decode");
        Assert.Null(t.Store.Get(id));
        Assert.Empty(t.Store.TakePendingEmbeddings(5));
    }

    [Fact]
    public void KeepKeyframe_false_deletes_image_after_embedding()
    {
        using var t = new TempStore();
        var id = AddReady(t.Store, T0, "No keyframe kept", Vec.Seeded(9), keep: false);
        Assert.Null(t.Store.GetKeyframe(id));
        Assert.False(t.Store.Get(id)!.HasThumb);
    }

    [Fact]
    public void Audio_is_text_searchable_at_once_and_gets_a_job()
    {
        using var t = new TempStore();
        var text = string.Concat(Enumerable.Repeat("filler words here ", 40)) + "the Acme budget meeting moved to Friday " + string.Concat(Enumerable.Repeat("more filler ", 40));
        var id = t.Store.AddAudioCapture(Caps.Audio(T0, text));
        var r = Find(t.Store, "budget", mode: SearchMode.Text).Results.Single();
        Assert.Equal(id, r.Id);
        Assert.Equal(CaptureKind.Audio, r.Kind);
        Assert.Equal(4, r.DurationS);
        Assert.True(r.Snippet.Length <= 300);
        Assert.Contains("budget", r.Snippet);
        var job = t.Store.TakePendingEmbeddings(5).Single();
        Assert.Equal(CaptureKind.Audio, job.Kind);
        Assert.Equal(text, job.Text);
        Assert.Equal(text, t.Store.Get(id)!.Text);
    }

    [Fact]
    public void Hybrid_ranking_fuses_text_and_vector()
    {
        using var t = new TempStore();
        var target = Vec.Seeded(100);
        var a = AddReady(t.Store, T0, "Acme invoice 4471", target);                 // text + vector match
        var b = AddReady(t.Store, T0.AddMinutes(1), "Acme newsletter", Vec.Seeded(200)); // text only
        var c = AddReady(t.Store, T0.AddMinutes(2), "Holiday photos", Vec.Near(target, 5)); // vector only
        for (int i = 0; i < 30; i++) AddReady(t.Store, T0.AddMinutes(3 + i), "Unrelated " + i, Vec.Seeded(1000 + i));
        var r = Find(t.Store, "acme invoice", target);
        Assert.Equal(a, r.Results[0].Id);
        Assert.Equal(1.0f, r.Results[0].Score, 3);
        Assert.Contains(r.Results, x => x.Id == b);
        Assert.Contains(r.Results, x => x.Id == c);
        Assert.All(r.Results, x => Assert.InRange(x.Score, 0f, 1f));
        Assert.True(r.Results.Zip(r.Results.Skip(1)).All(p => p.First.Score >= p.Second.Score));
        // Text mode skips vectors, vector mode skips text.
        Assert.DoesNotContain(Find(t.Store, "acme invoice", target, SearchMode.Text).Results, x => x.Id == c);
        Assert.Contains(Find(t.Store, "zzz", target, SearchMode.Vector).Results.Take(2), x => x.Id == c);
    }

    [Fact]
    public void Search_filters_time_app_and_kind()
    {
        using var t = new TempStore();
        var v = Vec.Seeded(7);
        var early = AddReady(t.Store, T0, "report alpha", v, "word.exe");
        var late = AddReady(t.Store, T0.AddHours(1), "report beta", v, "excel.exe");
        var audio = t.Store.AddAudioCapture(Caps.Audio(T0.AddMinutes(30), "the report is late", "teams.exe"));
        SearchResponse S(SearchRequest r) => t.Store.Search(r, v, 50, 1 << 20);
        Assert.Equal([late], S(new SearchRequest("report", From: T0.AddMinutes(45))).Results.Select(x => x.Id));
        Assert.Equal([early], S(new SearchRequest("report", App: "WORD.EXE")).Results.Select(x => x.Id));
        Assert.Equal([audio], S(new SearchRequest("report", Kind: [CaptureKind.Audio])).Results.Select(x => x.Id));
        Assert.Empty(S(new SearchRequest("report", App: "nothere.exe")).Results);
    }

    [Fact]
    public void MaxResponseBytes_trims_from_the_end_and_sets_truncated()
    {
        using var t = new TempStore();
        for (int i = 0; i < 40; i++) AddReady(t.Store, T0.AddSeconds(i), "Common title number " + i + new string('x', 200), Vec.Seeded(i));
        var full = t.Store.Search(new SearchRequest("common", 40), [], 40, 1 << 20);
        Assert.Equal(40, full.Count);
        Assert.False(full.Truncated);
        var cut = t.Store.Search(new SearchRequest("common", 40), [], 40, 4096);
        Assert.True(cut.Truncated);
        Assert.True(cut.Count < 40);
        Assert.Equal(full.Results.Take(cut.Count).Select(x => x.Id), cut.Results.Select(x => x.Id));
        var json = Envelope.Ok(System.Text.Json.JsonSerializer.Serialize(cut, CoreJson.Default.SearchResponse));
        Assert.True(Encoding.UTF8.GetByteCount(json) <= 4096);
    }

    [Fact]
    public void Purge_scopes_and_wal_truncate()
    {
        using var t = new TempStore();
        var now = DateTimeOffset.UtcNow;
        var old = AddReady(t.Store, now.AddHours(-3), "old one", Vec.Seeded(1), "a.exe");
        var mid = AddReady(t.Store, now.AddHours(-2), "mid one", Vec.Seeded(2), "b.exe");
        var recent = AddReady(t.Store, now.AddMinutes(-5), "recent one", Vec.Seeded(3), "a.exe");
        Assert.Equal(1, t.Store.CountPurge(PurgeScope.Last(TimeSpan.FromMinutes(10))));
        Assert.Equal(1, t.Store.Purge(PurgeScope.Last(TimeSpan.FromMinutes(10))));
        Assert.Null(t.Store.Get(recent));
        Assert.Equal(0, new FileInfo(DataPaths.StoreDb(t.Home.Path) + "-wal").Length);
        Assert.Equal(1, t.Store.Purge(PurgeScope.ForApp("B.EXE")));
        Assert.Null(t.Store.Get(mid));
        Assert.Equal(1, t.Store.Purge(PurgeScope.Range(now.AddHours(-4), now.AddHours(-2.5))));
        Assert.Null(t.Store.Get(old));
        Assert.Equal(0, t.Store.Stats().Rows);
        Assert.Equal(0, t.Store.Index.Count);
        Assert.Equal(0, t.Store.Purge(PurgeScope.Last(TimeSpan.Zero)));
    }

    [Fact]
    public void Retention_deletes_old_rows_and_old_month_files()
    {
        using var t = new TempStore();
        var now = DateTimeOffset.UtcNow;
        var ancient = AddReady(t.Store, now.AddDays(-100), "ancient", Vec.Seeded(1));
        var fresh = AddReady(t.Store, now.AddDays(-1), "fresh", Vec.Seeded(2));
        var oldMonth = now.AddDays(-100).UtcDateTime.ToString("yyyy-MM");
        Assert.True(File.Exists(Path.Combine(DataPaths.KeyframesDir(t.Home.Path), oldMonth + ".db")));
        t.Store.AppendAudit(new AuditEntry(now.AddDays(-100), "ipc", 1, "x", "search", 0, ""));
        var s = new Settings();
        s.Retention.Days = 30;
        t.Store.RunMaintenance(s);
        Assert.Null(t.Store.Get(ancient));
        Assert.NotNull(t.Store.Get(fresh));
        Assert.False(File.Exists(Path.Combine(DataPaths.KeyframesDir(t.Home.Path), oldMonth + ".db")));
        Assert.Empty(t.Store.ReadAudit(10));
    }

    [Fact]
    public void Embedding_space_dim_truncation_and_model_change()
    {
        using var t = new TempStore();
        var v = Vec.Seeded(11);
        var id = AddReady(t.Store, T0, "space test", v);
        t.Store.EnsureEmbeddingSpace(Vec.Info);
        Assert.Equal(0, t.Store.Stats().PendingEmbeddings);

        // Lower dim: stored vectors truncated in place, no re-embed.
        t.Store.EnsureEmbeddingSpace(Vec.Info with { Dim = 256 });
        Assert.Equal(0, t.Store.Stats().PendingEmbeddings);
        var r = t.Store.Search(new SearchRequest("q", Mode: SearchMode.Vector), v.AsSpan(0, 256), 10, 1 << 20);
        Assert.Equal(id, r.Results.Single().Id);
        // A full 768-d query is truncated to the row's 256-d.
        Assert.Equal(id, t.Store.Search(new SearchRequest("q", Mode: SearchMode.Vector), v, 10, 1 << 20).Results.Single().Id);

        // Model change: rows with a keyframe or text get a job again; vector search ignores the old space.
        var noKf = AddReady(t.Store, T0.AddMinutes(1), "no keyframe", Vec.Seeded(12), keep: false);
        t.Store.EnsureEmbeddingSpace(new EmbeddingInfo("siglip2", "r1", 768));
        Assert.Equal(1, t.Store.Stats().PendingEmbeddings);
        Assert.Equal(id, t.Store.TakePendingEmbeddings(10).Single().Id);
        Assert.Empty(t.Store.Search(new SearchRequest("q", Mode: SearchMode.Vector), v, 10, 1 << 20).Results);
        Assert.NotNull(t.Store.Get(noKf)); // still visible, still text-searchable
        Assert.Single(t.Store.Search(new SearchRequest("keyframe", Mode: SearchMode.Text), [], 10, 1 << 20).Results);
    }

    [Fact]
    public void Audit_round_trip_newest_first()
    {
        using var t = new TempStore();
        t.Store.AppendAudit(new AuditEntry(DateTimeOffset.UtcNow, "ipc", 10, @"C:\a.exe", "connect", 0, ""));
        t.Store.AppendAudit(new AuditEntry(DateTimeOffset.UtcNow, "mcp", 11, @"C:\b.exe", "search", 3, "abc"));
        var a = t.Store.ReadAudit(10);
        Assert.Equal("search", a[0].Verb);
        Assert.Equal("abc", a[0].QuerySha256);
        Assert.Equal(2, a.Count);
    }

    [Fact]
    public void Concurrent_writes_and_searches_are_safe()
    {
        using var t = new TempStore();
        var tasks = Enumerable.Range(0, 4).Select(w => Task.Run(() =>
        {
            for (int i = 0; i < 30; i++)
            {
                var id = t.Store.AddScreenCapture(Caps.Screen(T0.AddSeconds(w * 100 + i), title: $"worker {w} item {i}"));
                t.Store.SetEmbedding(id, Vec.Seeded(w * 1000 + i), Vec.Info);
                t.Store.Search(new SearchRequest("worker"), Vec.Seeded(i), 10, 1 << 20);
            }
        })).ToArray();
        Task.WaitAll(tasks);
        Assert.Equal(120, t.Store.Stats().Rows);
        Assert.Equal(120, t.Store.Index.Count);
    }

    [Fact]
    public void Fts_query_escapes_operators()
    {
        Assert.Equal("\"a\" OR \"AND\" OR \"bc\"*", EncryptedStore.FtsQuery("a AND bc"));
        Assert.Null(EncryptedStore.FtsQuery("\"*() :"));
        using var t = new TempStore();
        AddReady(t.Store, T0, "C# NEAR(x) title", Vec.Seeded(1));
        Assert.Single(Find(t.Store, "NEAR( \" OR", mode: SearchMode.Text).Results);
    }

    private static string AddSite(EncryptedStore s, DateTimeOffset ts, string title, float[] v, string site, string app = "chrome.exe")
    {
        var id = s.AddScreenCapture(Caps.Screen(ts, app, title) with { Site = site });
        s.TakePendingEmbeddings(10);
        s.SetEmbedding(id, v, Vec.Info);
        return id;
    }

    [Fact]
    public void Site_is_stored_normalised_and_filters_text_and_vector_search()
    {
        using var t = new TempStore();
        var gh = AddSite(t.Store, T0, "Pull request 12 - review", Vec.Seeded(1), "WWW.GitHub.com");
        var gl = AddSite(t.Store, T0.AddMinutes(1), "Merge request 12 - review", Vec.Seeded(2), "gitlab.com");
        AddReady(t.Store, T0.AddMinutes(2), "review notes", Vec.Seeded(3), "Code.exe");
        Assert.Equal("github.com", t.Store.Get(gh)!.Site);
        Assert.Equal("github.com", t.Store.Timeline(T0.AddMinutes(-1), T0.AddMinutes(5), 10).Single(m => m.Id == gh).Site);

        var text = t.Store.Search(new SearchRequest("review", 50, Mode: SearchMode.Text, Site: "github.com"), [], 50, 1 << 20);
        Assert.Equal([gh], text.Results.Select(h => h.Id));
        Assert.Equal("github.com", text.Results[0].Site);
        var vec = t.Store.Search(new SearchRequest("x", 50, Mode: SearchMode.Vector, Site: "gitlab.com"), Vec.Seeded(1), 50, 1 << 20);
        Assert.Equal([gl], vec.Results.Select(h => h.Id));
        Assert.Empty(t.Store.Search(new SearchRequest("review", 50, Site: "nowhere.org"), Vec.Seeded(1), 50, 1 << 20).Results);

        t.Reopen();   // the vector index is rebuilt from disk with the site
        vec = t.Store.Search(new SearchRequest("x", 50, Mode: SearchMode.Vector, Site: "github.com"), Vec.Seeded(2), 50, 1 << 20);
        Assert.Equal([gh], vec.Results.Select(h => h.Id));
    }

    [Fact]
    public void Facets_count_apps_and_sites_in_the_window()
    {
        using var t = new TempStore();
        AddSite(t.Store, T0, "a", Vec.Seeded(1), "github.com");
        AddSite(t.Store, T0.AddMinutes(1), "b", Vec.Seeded(2), "github.com");
        AddSite(t.Store, T0.AddMinutes(2), "c", Vec.Seeded(3), "gitlab.com");
        AddReady(t.Store, T0.AddMinutes(3), "d", Vec.Seeded(4), "Code.exe");
        t.Store.AddAudioCapture(Caps.Audio(T0.AddHours(-30), "old standup", "teams.exe"));
        var f = t.Store.Facets(null, null, 10);
        Assert.Equal([("chrome.exe", 3), ("Code.exe", 1), ("teams.exe", 1)], f.Apps.Select(a => (a.Name, a.Count)));
        Assert.Equal([("github.com", 2), ("gitlab.com", 1)], f.Sites.Select(a => (a.Name, a.Count)));
        f = t.Store.Facets(T0.AddMinutes(1), T0.AddHours(1), 10);
        Assert.Equal([("chrome.exe", 2), ("Code.exe", 1)], f.Apps.Select(a => (a.Name, a.Count)));
        Assert.Single(t.Store.Facets(null, null, 1).Apps);
    }

    [Fact]
    public void Purge_by_id_and_site()
    {
        using var t = new TempStore();
        var a = AddSite(t.Store, T0, "a", Vec.Seeded(1), "github.com");
        var b = AddSite(t.Store, T0.AddMinutes(1), "b", Vec.Seeded(2), "github.com");
        var c = AddSite(t.Store, T0.AddMinutes(2), "c", Vec.Seeded(3), "gitlab.com");
        Assert.Equal(1, t.Store.CountPurge(PurgeScope.ForId(a)));
        Assert.Equal(1, t.Store.Purge(PurgeScope.ForId(a)));
        Assert.Null(t.Store.Get(a));
        Assert.Equal(1, t.Store.Purge(PurgeScope.ForSite("WWW.github.com")));
        Assert.Null(t.Store.Get(b));
        Assert.NotNull(t.Store.Get(c));
        Assert.Equal(0, t.Store.Purge(PurgeScope.ForId("s_nope")));
    }

    [Fact]
    public void Search_history_is_newest_first_deduplicated_capped_and_encrypted()
    {
        using var t = new TempStore();
        var now = DateTimeOffset.UtcNow;
        t.Store.AddHistory("acme invoice", now.AddMinutes(-3), 3);
        t.Store.AddHistory("tokyo flight", now.AddMinutes(-2), 3);
        t.Store.AddHistory("  acme invoice ", now.AddMinutes(-1), 3);   // bumps, no duplicate
        Assert.Equal(["acme invoice", "tokyo flight"], t.Store.History(10).Select(h => h.Q));
        t.Store.AddHistory("standup", now, 3);
        t.Store.AddHistory("budget", now.AddSeconds(1), 3);
        Assert.Equal(["budget", "standup", "acme invoice"], t.Store.History(10).Select(h => h.Q));
        t.Store.Checkpoint();
        Assert.DoesNotContain("tokyo", Encoding.UTF8.GetString(Raw(DataPaths.StoreDb(t.Home.Path))));
        Assert.DoesNotContain("budget", Encoding.UTF8.GetString(Raw(DataPaths.StoreDb(t.Home.Path))));

        Assert.Equal(1, t.Store.DeleteHistory("standup"));
        Assert.Equal(["budget", "acme invoice"], t.Store.History(10).Select(h => h.Q));
        // A time-window purge forgets searches typed in that window.
        t.Store.Purge(PurgeScope.Range(now.AddSeconds(-30), now.AddMinutes(1)));
        Assert.Equal(["acme invoice"], t.Store.History(10).Select(h => h.Q));
        t.Reopen();
        Assert.Single(t.Store.History(10));
        Assert.Equal(1, t.Store.DeleteHistory(null));
        Assert.Empty(t.Store.History(10));
    }

    [Fact]
    public void Schema_1_store_gains_site_and_history_on_open()
    {
        using var t = new TempStore();
        var id = AddReady(t.Store, T0, "Acme invoice", Vec.Seeded(1));
        t.Store.Dispose();
        using (var c = new SqliteConnection(new SqliteConnectionStringBuilder { DataSource = DataPaths.StoreDb(t.Home.Path), Pooling = false }.ToString()))
        {
            c.Open();
            foreach (var sql in new[] { "PRAGMA cipher='sqlcipher'", "PRAGMA legacy=4", "PRAGMA key=\"x'" + Convert.ToHexString(t.Key) + "'\"",
                "DROP INDEX ix_captures_site", "ALTER TABLE captures DROP COLUMN site", "DROP TABLE search_history", "UPDATE meta SET v='1' WHERE k='schema'" })
            {
                using var cmd = c.CreateCommand();
                cmd.CommandText = sql;
                cmd.ExecuteNonQuery();
            }
        }
        t.Reopen();
        Assert.Equal("", t.Store.Get(id)!.Site);
        Assert.Single(Find(t.Store, "acme", Vec.Seeded(1)).Results);
        t.Store.AddHistory("acme", DateTimeOffset.UtcNow, 10);
        Assert.Single(t.Store.History(10));
    }
}
