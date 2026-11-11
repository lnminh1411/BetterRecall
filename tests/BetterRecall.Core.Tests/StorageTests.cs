// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.IO.Compression;
using System.Text.Json;
using BetterRecall.Contracts;
using BetterRecall.Core.Store;

namespace BetterRecall.Core.Tests;

public class StorageSettingsTests
{
    private static JsonElement J(string json) => JsonDocument.Parse(json).RootElement.Clone();

    [Fact]
    public void Defaults_of_batch_2()
    {
        var s = new Settings();
        Assert.Equal("avif", s.Keyframes.Format);
        Assert.Equal(15, s.Capture.DirtyMinPercent);
        Assert.Equal(25, s.Storage.MinFreeGb);
        Assert.Equal(20, s.Storage.MaxGb);
        Assert.False(s.Storage.Overwrite);
        Assert.Equal(7, s.Retention.ProgressiveAfterDays);
        Assert.Equal(30, s.Retention.ProgressiveOverDays);
        Assert.Null(SettingsStore.Validate(s));
    }

    [Fact]
    public void Old_file_moves_keyframes_maxGb_and_old_defaults()
    {
        using var h = new TempHome(null);
        File.WriteAllText(DataPaths.SettingsFile(h.Path),
            "{\"capture\":{\"dirtyMinPercent\":0.5,\"intervalMs\":3000},\"keyframes\":{\"format\":\"webp\",\"maxGb\":55,\"quality\":80}}");
        var s = SettingsStore.Load(h.Path).Current;
        Assert.Equal(55, s.Storage.MaxGb);
        Assert.Equal("avif", s.Keyframes.Format);
        Assert.Equal(15, s.Capture.DirtyMinPercent);
        Assert.Equal(3000, s.Capture.IntervalMs);
        Assert.Equal(80, s.Keyframes.Quality);
        var text = File.ReadAllText(DataPaths.SettingsFile(h.Path));
        Assert.DoesNotContain("\"maxGb\": 55,\n    \"thumbCacheMb\"", text.Replace("\r", ""));
        Assert.Contains("\"storage\": {", text);
        // Second load: nothing left to migrate.
        Assert.Equal(55, SettingsStore.Load(h.Path).Current.Storage.MaxGb);
    }

    [Fact]
    public void Old_file_keeps_values_the_user_changed()
    {
        using var h = new TempHome(null);
        File.WriteAllText(DataPaths.SettingsFile(h.Path),
            "{\"capture\":{\"dirtyMinPercent\":2},\"keyframes\":{\"format\":\"jpeg\",\"maxGb\":20}}");
        var s = SettingsStore.Load(h.Path).Current;
        Assert.Equal(2, s.Capture.DirtyMinPercent);
        Assert.Equal("jpeg", s.Keyframes.Format);
        Assert.Equal(20, s.Storage.MaxGb);
    }

    [Theory]
    [InlineData("{\"keyframes\":{\"maxGb\":20}}", "Invalid keyframes.maxGb.")]
    [InlineData("{\"keyframes\":{\"format\":\"heic\"}}", "Invalid keyframes.format.")]
    [InlineData("{\"keyframes\":{\"encoder\":\"gpu\"}}", "Invalid keyframes.encoder.")]
    [InlineData("{\"storage\":{\"minFreeGb\":-1}}", "Invalid storage.minFreeGb.")]
    [InlineData("{\"storage\":{\"maxGb\":0}}", "Invalid storage.maxGb.")]
    [InlineData("{\"storage\":{\"overwrite\":1}}", "Invalid storage.overwrite.")]
    [InlineData("{\"retention\":{\"progressiveAfterDays\":-1}}", "Invalid retention.progressiveAfterDays.")]
    [InlineData("{\"retention\":{\"progressiveOverDays\":0}}", "Invalid retention.progressiveOverDays.")]
    public void Invalid_storage_keys_are_rejected(string json, string error)
    {
        using var h = new TempHome(null);
        var s = SettingsStore.Load(h.Path);
        Assert.Equal(error, s.TryUpdate(J(json)));
    }

    [Theory]
    [InlineData("{\"keyframes\":{\"format\":\"jxl\"}}")]
    [InlineData("{\"storage\":{\"minFreeGb\":0,\"maxGb\":500,\"overwrite\":true}}")]
    [InlineData("{\"retention\":{\"progressiveAfterDays\":0,\"progressiveOverDays\":3650}}")]
    public void Valid_storage_keys_are_accepted(string json)
    {
        using var h = new TempHome(null);
        Assert.Null(SettingsStore.Load(h.Path).TryUpdate(J(json)));
    }
}

public class StoragePolicyStoreTests
{
    private static readonly DateTimeOffset Now = new(2026, 11, 12, 12, 0, 0, TimeSpan.Zero);

    /// <summary>Ready screen row with an explicit change score and switch flag.</summary>
    private static string Add(EncryptedStore s, DateTimeOffset ts, double? change, bool sw, string app = "code", string title = "main.cs", byte[]? image = null, int monitor = 0)
    {
        var id = s.AddScreenCapture(new NewScreenCapture(ts, app, @"C:\" + app + ".exe", title, monitor, 1280, 720, 0x1234,
            image ?? Caps.FakeWebp, "image/webp", true, "", change, sw));
        s.TakePendingEmbeddings(10);
        s.SetEmbedding(id, Vec.Seeded(ts.Second + ts.Minute * 60), Vec.Info);
        return id;
    }

    [Fact]
    public void Change_score_and_switch_flag_are_stored()
    {
        using var t = new TempStore();
        var a = Add(t.Store, Now, 42.5, true);
        var b = t.Store.AddScreenCapture(Caps.Screen(Now.AddSeconds(2)));
        Assert.Equal((42.5, true), t.Store.StoredScore(a));
        Assert.Equal((null, null), t.Store.StoredScore(b));   // no score from the caller: unknown
        t.Reopen(); // migration is idempotent
        Assert.Equal((42.5, true), t.Store.StoredScore(a));
    }

    [Fact]
    public void Thin_target_follows_the_period()
    {
        Assert.Equal(10, EncryptedStore.ThinTarget(10, 0));
        Assert.Equal(5, EncryptedStore.ThinTarget(10, 0.5));
        Assert.Equal(1, EncryptedStore.ThinTarget(10, 0.95));
        Assert.Equal(0, EncryptedStore.ThinTarget(10, 1));
    }

    [Fact]
    public void Thinning_drops_smallest_changes_first_and_keeps_switches()
    {
        using var t = new TempStore();
        var r = new RetentionSettings { Days = 365, ProgressiveAfterDays = 7, ProgressiveOverDays = 10 };
        // The day's midpoint is 13 days old: p = (13 - 7) / 10 = 0.6, so 40 % of 20 non-switch rows survive (8).
        var day = new DateTimeOffset(Now.UtcDateTime.Date, TimeSpan.Zero).AddDays(-13);
        var ids = new List<(string Id, double Change)>();
        for (int i = 0; i < 20; i++) ids.Add((Add(t.Store, day.AddMinutes(i + 1), i * 5.0, false), i * 5.0));
        var sw = Add(t.Store, day.AddMinutes(30), 0.1, true);
        var fresh = Add(t.Store, Now.AddDays(-1), 0.1, false);
        int deleted = t.Store.ThinForTest(r, Now);
        Assert.Equal(12, deleted);
        foreach (var (id, change) in ids)
            Assert.Equal(change >= 60, t.Store.Get(id) is not null);
        Assert.NotNull(t.Store.Get(sw));
        Assert.NotNull(t.Store.Get(fresh));
        // Same clock again: the day's total is remembered, nothing more goes.
        Assert.Equal(0, t.Store.ThinForTest(r, Now));
        // At the end of the period only the switch is left.
        Assert.Equal(8, t.Store.ThinForTest(r, Now.AddDays(5)));
        Assert.NotNull(t.Store.Get(sw));
    }

    [Fact]
    public void Thinning_off_and_legacy_rows_scored_from_dhash()
    {
        using var t = new TempStore();
        var day = new DateTimeOffset(Now.UtcDateTime.Date, TimeSpan.Zero).AddDays(-20);
        var legacy = new List<string>();
        // Rows from before the change score: same app and title, so only the first counts as a switch.
        for (int i = 0; i < 4; i++)
        {
            var id = t.Store.AddScreenCapture(Caps.Screen(day.AddMinutes(i + 1), "code", "same"));
            t.Store.TakePendingEmbeddings(10);
            t.Store.SetEmbedding(id, Vec.Seeded(i), Vec.Info);
            legacy.Add(id);
        }
        Assert.Equal(0, t.Store.ThinForTest(new RetentionSettings { Days = 365, ProgressiveAfterDays = 0 }, Now));
        Assert.Equal(3, t.Store.ThinForTest(new RetentionSettings { Days = 365, ProgressiveAfterDays = 1, ProgressiveOverDays = 1 }, Now));
        Assert.NotNull(t.Store.Get(legacy[0]));
    }

    [Fact]
    public void Delete_for_space_goes_oldest_day_first_in_progressive_order()
    {
        using var t = new TempStore();
        var d1 = new DateTimeOffset(Now.UtcDateTime.Date, TimeSpan.Zero).AddDays(-3);
        var oldSwitch = Add(t.Store, d1.AddHours(1), 90, true);
        var oldBig = Add(t.Store, d1.AddHours(2), 80, false);
        var oldSmall = Add(t.Store, d1.AddHours(3), 1, false);
        var audio = t.Store.AddAudioCapture(Caps.Audio(d1.AddHours(4), "hello there"));
        var newer = Add(t.Store, Now.AddHours(-1), 0, false);

        Assert.Equal(1, t.Store.DeleteForSpace(1));
        Assert.Null(t.Store.Get(oldSmall));
        Assert.Equal(1, t.Store.DeleteForSpace(1));
        Assert.Null(t.Store.Get(oldBig));
        Assert.Equal(2, t.Store.DeleteForSpace(2)); // then switches and audio by age
        Assert.Null(t.Store.Get(oldSwitch));
        Assert.Null(t.Store.Get(audio));
        Assert.NotNull(t.Store.Get(newer));
        Assert.Equal(1, t.Store.DeleteForSpace(10));
        Assert.Equal(0, t.Store.DeleteForSpace(10));
    }

    [Fact]
    public void Storage_use_counts_free_pages_and_empty_month_files_go()
    {
        using var t = new TempStore();
        var big = new byte[300_000];
        new Random(3).NextBytes(big);
        var oldMonth = Now.AddDays(-70);
        for (int i = 0; i < 5; i++) Add(t.Store, oldMonth.AddMinutes(i), i, false, image: big);
        for (int i = 0; i < 5; i++) Add(t.Store, Now.AddMinutes(-i - 1), i, false, image: big);
        t.Store.Checkpoint();                    // WAL into the files, so sizes compare like for like
        var before = t.Store.GetStorageUse();
        Assert.True(before.LiveBytes > 2_500_000);
        var oldFile = Path.Combine(DataPaths.KeyframesDir(t.Home.Path), oldMonth.UtcDateTime.ToString("yyyy-MM") + ".db");
        Assert.True(File.Exists(oldFile));
        Assert.Equal(5, t.Store.DeleteForSpace(5));
        Assert.False(File.Exists(oldFile));      // whole month gone: file deleted
        Assert.Equal(2, t.Store.DeleteForSpace(2));
        t.Store.Checkpoint();
        var after = t.Store.GetStorageUse();
        Assert.True(after.LiveBytes < before.LiveBytes - 1_800_000, $"{after.LiveBytes} vs {before.LiveBytes}");
        // New month files use incremental vacuum, so deleted keyframes leave the file at once.
        Assert.True(after.FileBytes < before.FileBytes - 1_800_000, $"{after.FileBytes} vs {before.FileBytes}");
    }

    [Fact]
    public void Maintenance_runs_thinning_with_settings()
    {
        using var t = new TempStore();
        var old = DateTimeOffset.UtcNow.AddDays(-25);
        var a = Add(t.Store, old, 1, false);
        var b = Add(t.Store, old.AddMinutes(1), 50, true);
        var s = new Settings();
        s.Retention.Days = 30;
        s.Retention.ProgressiveAfterDays = 7;
        s.Retention.ProgressiveOverDays = 10; // fully thinned by day 17
        t.Store.RunMaintenance(s);
        Assert.Null(t.Store.Get(a));
        Assert.NotNull(t.Store.Get(b));
    }
}

public class ExportTests
{
    private sealed class JpegThumbs : IThumbnailer
    {
        public int Calls;
        public Task<ThumbData?> GetThumbAsync(string id, int width, CancellationToken ct) => Task.FromResult<ThumbData?>(null);
        public void ClearCache() { }
        public ThumbData Transcode(ThumbData keyframe, string target)
        {
            Calls++;
            return target == "jpeg" ? keyframe with { Mime = "image/jpeg", Bytes = [0xFF, 0xD8, 1, 2] } : keyframe;
        }
    }

    [Fact]
    public void Writes_images_and_captures_json_filtered_by_app()
    {
        using var t = new TempStore();
        var now = DateTimeOffset.UtcNow;
        var ids = new List<string>();
        for (int i = 0; i < 5; i++)
        {
            var id = t.Store.AddScreenCapture(Caps.Screen(now.AddMinutes(-10 + i), i % 2 == 0 ? "code" : "chrome", "Title " + i));
            t.Store.TakePendingEmbeddings(10);
            t.Store.SetEmbedding(id, Vec.Seeded(i), Vec.Info);
            ids.Add(id);
        }
        t.Store.AddAudioCapture(Caps.Audio(now.AddMinutes(-3), "meeting notes", "code"));
        var outside = t.Store.AddScreenCapture(Caps.Screen(now.AddDays(-2), "code"));
        t.Store.TakePendingEmbeddings(10);
        t.Store.SetEmbedding(outside, Vec.Seeded(9), Vec.Info);

        var path = Path.Combine(t.Home.Path, "export.zip");
        var req = new ExportRequest(now.AddHours(-1), now, path, App: "CODE.exe");
        Assert.Equal(new ExportResult(4, 3, 0), CaptureExporter.Count(t.Store, req));
        var thumbs = new JpegThumbs();
        var r = CaptureExporter.Write(t.Store, req, thumbs.Transcode, CancellationToken.None);
        Assert.Equal(4, r.Count);
        Assert.Equal(3, r.Images);
        Assert.Equal(0, thumbs.Calls);
        Assert.False(File.Exists(path + ".partial"));
        using (var zip = ZipFile.OpenRead(path))
        {
            Assert.Equal(4, zip.Entries.Count); // 3 images + captures.json
            var json = JsonDocument.Parse(zip.GetEntry("captures.json")!.Open());
            var items = json.RootElement.EnumerateArray().ToList();
            Assert.Equal(4, items.Count);
            Assert.Equal("Title 0", items[0].GetProperty("title").GetString());   // oldest first
            Assert.EndsWith(".webp", items[0].GetProperty("file").GetString());
            Assert.Equal("audio", items[3].GetProperty("kind").GetString());
            Assert.Equal("meeting notes", items[3].GetProperty("text").GetString());
            Assert.False(items[3].TryGetProperty("file", out _));
            var img = zip.GetEntry(items[0].GetProperty("file").GetString()!)!;
            using var ms = new MemoryStream();
            img.Open().CopyTo(ms);
            Assert.Equal(Caps.FakeWebp, ms.ToArray());
        }

        var jpegPath = Path.Combine(t.Home.Path, "jpeg.zip");
        r = CaptureExporter.Write(t.Store, req with { Path = jpegPath, Jpeg = true }, thumbs.Transcode, CancellationToken.None);
        Assert.Equal(3, thumbs.Calls);
        using (var zip = ZipFile.OpenRead(jpegPath))
            Assert.Equal(3, zip.Entries.Count(e => e.FullName.EndsWith(".jpg", StringComparison.Ordinal)));
    }

    [Fact]
    public void Paging_covers_more_rows_than_one_timeline_page()
    {
        using var t = new TempStore();
        var now = DateTimeOffset.UtcNow;
        var info = Vec.Info;
        t.Store.InsertSynthetic(Enumerable.Range(0, 4500).Select(i => (now.AddSeconds(-i - 1), "app", "t" + i, Vec.Seeded(i % 50))), info);
        var r = CaptureExporter.Count(t.Store, new ExportRequest(now.AddDays(-1), now, ""));
        Assert.Equal(4500, r.Count);
        Assert.Equal(0, r.Images);
    }
}
