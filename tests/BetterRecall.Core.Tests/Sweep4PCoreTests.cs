// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Text.Json;
using BetterRecall.Contracts;
using BetterRecall.Core.Ipc;
using BetterRecall.Core.Store;

namespace BetterRecall.Core.Tests;

/// <summary>Bug sweep 4, batch P (purge and export fences), Core side.</summary>
public sealed class Sweep4PCoreTests : IDisposable
{
    private readonly TempStore _t = new();
    private readonly TempHome _out = new(null);
    private static readonly byte[] Image = [.. "RIFF"u8, 0, 0, 0, 0, .. "WEBPVP8 sweep4-image-bytes"u8];

    public void Dispose()
    {
        _t.Dispose();
        _out.Dispose();
    }

    private static string Screen(IStore s, DateTimeOffset ts, string app = "chrome.exe", string title = "row", int seed = 1)
    {
        var id = s.AddScreenCapture(Caps.Screen(ts, app, title, image: Image));
        s.SetEmbedding(id, Vec.Seeded(seed), Vec.Info);
        return id;
    }

    // ---- A-H1: the store opened by purge all refuses what was captured before it

    [Fact]
    public async Task A_H1_store_after_purge_all_refuses_captures_stamped_before_it()
    {
        using var h = new TempHome("dpapi");
        var host = (CoreHostImpl)CoreHost.Create(new CoreHostOptions(h.Path, h.Path));
        await using (host)
        {
            await host.OpenStoreAsync(CancellationToken.None);
            var grabbed = DateTimeOffset.UtcNow.AddSeconds(-3);   // a frame in OCR, a segment in the worker
            await host.PurgeAllAsync(CancellationToken.None);
            var fresh = host.Store!;
            Assert.Throws<PurgedWindowException>(() => fresh.AddScreenCapture(Caps.Screen(grabbed)));
            Assert.Throws<PurgedWindowException>(() => fresh.AddAudioCapture(Caps.Audio(grabbed, "pre purge marker alpha")));
            Assert.Throws<PurgedWindowException>(() => fresh.AddReplayFrame(new NewReplayFrame(grabbed, "r1", 0, "chrome", "t", 0, 64, 36, Image, "image/webp")));
            Assert.Equal(0, host.EncryptedStore!.CountPurge(new PurgeScope("all")));
            Assert.NotNull(fresh.AddScreenCapture(Caps.Screen(DateTimeOffset.UtcNow.AddSeconds(1))));
        }
    }

    [Fact]
    public async Task A_H1_purge_all_notice_comes_before_the_new_store_opens()
    {
        using var h = new TempHome("dpapi");
        var host = (CoreHostImpl)CoreHost.Create(new CoreHostOptions(h.Path, h.Path));
        await using (host)
        {
            await host.OpenStoreAsync(CancellationToken.None);
            var oldDb = DataPaths.StoreDb(h.Path);
            int notices = 0;
            bool noStore = false, filesStillThere = false;
            ((ICoreHost)host).Purged += n =>
            {
                if (n.Reason != "all") return;
                notices++;
                noStore = host.Store is null;          // nothing accepts writes while in-RAM state is dropped
                filesStillThere = File.Exists(oldDb);  // told before any file goes: a replay save in flight is discarded first
            };
            await host.PurgeAllAsync(CancellationToken.None);
            Assert.Equal(1, notices);
            Assert.True(noStore);
            Assert.True(filesStillThere);
            Assert.NotNull(host.Store);
        }
    }

    // ---- C1 + BC-S2: purge all stops a running capture export with the purge message

    [Fact]
    public async Task C1_purge_all_during_the_image_phase_aborts_the_export()
    {
        using var h = new TempHome("dpapi");
        var host = (CoreHostImpl)CoreHost.Create(new CoreHostOptions(h.Path, h.Path));
        await using (host)
        {
            await host.OpenStoreAsync(CancellationToken.None);
            Screen(host.Store!, DateTimeOffset.UtcNow.AddMinutes(-4), seed: 3);
            Screen(host.Store!, DateTimeOffset.UtcNow.AddMinutes(-3), seed: 4);
            var path = Path.Combine(_out.Path, "img.zip");
            int calls = 0;
            ThumbData Transcode(ThumbData kf, string target)
            {
                if (calls++ == 0) host.PurgeAllAsync(CancellationToken.None).GetAwaiter().GetResult();
                return kf;
            }
            // Used to fail with ObjectDisposedException (answered "Internal error.") on the next store read.
            Assert.Throws<ExportPurgedException>(() => CaptureExporter.Write(host.Store!,
                new ExportRequest(DateTimeOffset.UtcNow.AddHours(-1), DateTimeOffset.UtcNow, path, Jpeg: true), Transcode, CancellationToken.None));
            Assert.False(File.Exists(path));
            Assert.False(File.Exists(path + ".partial"));
        }
    }

    [Fact]
    public async Task C1_purge_all_during_the_json_phase_does_not_publish_the_zip()
    {
        using var h = new TempHome("dpapi");
        var host = (CoreHostImpl)CoreHost.Create(new CoreHostOptions(h.Path, h.Path));
        await using (host)
        {
            await host.OpenStoreAsync(CancellationToken.None);
            Screen(host.Store!, DateTimeOffset.UtcNow.AddMinutes(-4), seed: 5);
            var path = Path.Combine(_out.Path, "json.zip");
            // The last row's transcode is the last store work: purge all lands while captures.json is written.
            ThumbData Transcode(ThumbData kf, string target)
            {
                host.PurgeAllAsync(CancellationToken.None).GetAwaiter().GetResult();
                return kf;
            }
            Assert.Throws<ExportPurgedException>(() => CaptureExporter.Write(host.Store!,
                new ExportRequest(DateTimeOffset.UtcNow.AddHours(-1), DateTimeOffset.UtcNow, path, Jpeg: true), Transcode, CancellationToken.None));
            Assert.False(File.Exists(path));
            Assert.False(File.Exists(path + ".partial"));
        }
    }

    [Fact]
    public async Task BC_S2_export_verb_answers_the_purge_message_when_purge_all_runs_during_it()
    {
        // Seam: the real CommandProcessor export verb, the real exporter and store, purge all of the real CoreHost.
        using var h = new TempHome("dpapi");
        var host = (CoreHostImpl)CoreHost.Create(new CoreHostOptions(h.Path, h.Path));
        await using (host)
        {
            await host.OpenStoreAsync(CancellationToken.None);
            Screen(host.Store!, DateTimeOffset.UtcNow.AddMinutes(-4), seed: 6);
            Screen(host.Store!, DateTimeOffset.UtcNow.AddMinutes(-3), seed: 7);
            var entered = new ManualResetEventSlim();
            var release = new ManualResetEventSlim();
            var thumbs = new GateThumbs(entered, release);
            var keys = new FakeKeys();
            var p = new CommandProcessor(new CommandDeps
            {
                Settings = host.Settings,
                Store = () => host.Store,
                Keys = keys,
                Agent = new FakeAgent(),
                Embedder = new FakeEmbedder(),
                Thumbnailer = thumbs,
                PurgeAll = host.PurgeAllAsync,
            });
            var ui = new ClientSession(PipeKind.Ui, 101, @"C:\app\ui\BetterRecall.UI.exe");
            var path = Path.Combine(_out.Path, "verb.zip");
            var json = JsonSerializer.Serialize(new ExportRequest(DateTimeOffset.UtcNow.AddHours(-1), DateTimeOffset.UtcNow, path, Jpeg: true), ContractsJson.Default.ExportRequest);
            var export = Task.Run(() => p.HandleAsync(ui, "export " + json, CancellationToken.None));
            Assert.True(entered.Wait(TimeSpan.FromSeconds(20)));
            await host.PurgeAllAsync(CancellationToken.None);
            release.Set();
            var reply = await export.WaitAsync(TimeSpan.FromSeconds(20));
            var (ok, _, error) = Envelope.Parse(reply.Line);
            Assert.False(ok);
            Assert.Equal(new ExportPurgedException().Message, error);
            Assert.False(File.Exists(path));
            Assert.False(File.Exists(path + ".partial"));
        }
    }

    private sealed class GateThumbs(ManualResetEventSlim entered, ManualResetEventSlim release) : IThumbnailer
    {
        public Task<ThumbData?> GetThumbAsync(string id, int width, CancellationToken ct) => Task.FromResult<ThumbData?>(null);
        public void ClearCache() { }
        public ThumbData Transcode(ThumbData keyframe, string target)
        {
            entered.Set();
            release.Wait(TimeSpan.FromSeconds(30));
            return keyframe;
        }
    }

    // ---- A-M2: app and site purges fence what is still in flight

    [Fact]
    public void A_M2_app_purge_refuses_frames_of_that_app_grabbed_before_it()
    {
        var before = DateTimeOffset.UtcNow.AddSeconds(-25);
        Screen(_t.Store, before.AddMinutes(-10), app: "notepad.exe");
        _t.Store.Purge(PurgeScope.ForApp("notepad"));
        Assert.Throws<PurgedWindowException>(() => _t.Store.AddScreenCapture(Caps.Screen(before, "notepad.exe")));
        Assert.Throws<PurgedWindowException>(() => _t.Store.AddScreenCapture(Caps.Screen(before, "Notepad")));
        Assert.Throws<PurgedWindowException>(() => _t.Store.AddAudioCapture(Caps.Audio(before, "said in notepad", app: "notepad")));
        Assert.Throws<PurgedWindowException>(() => _t.Store.AddReplayFrame(new NewReplayFrame(before, "r1", 0, "notepad", "t", 0, 64, 36, Image, "image/webp")));
        // Other apps, and the purged app after the purge, are kept.
        Assert.NotNull(_t.Store.AddScreenCapture(Caps.Screen(before, "chrome.exe")));
        Assert.NotNull(_t.Store.AddScreenCapture(Caps.Screen(DateTimeOffset.UtcNow.AddSeconds(1), "notepad.exe")));
    }

    [Fact]
    public void A_M2_site_purge_refuses_frames_of_that_site_grabbed_before_it()
    {
        var before = DateTimeOffset.UtcNow.AddSeconds(-5);
        _t.Store.Purge(PurgeScope.ForSite("www.Example.com"));
        Assert.Throws<PurgedWindowException>(() => _t.Store.AddScreenCapture(Caps.Screen(before, "msedge.exe") with { Site = "example.com" }));
        Assert.Throws<PurgedWindowException>(() => _t.Store.AddReplayFrame(new NewReplayFrame(before, "r1", 0, "msedge", "t", 0, 64, 36, Image, "image/webp", Site: "www.example.com")));
        Assert.NotNull(_t.Store.AddScreenCapture(Caps.Screen(before, "msedge.exe") with { Site = "other.org" }));
        Assert.NotNull(_t.Store.AddScreenCapture(Caps.Screen(DateTimeOffset.UtcNow.AddSeconds(1), "msedge.exe") with { Site = "example.com" }));
    }

    [Fact]
    public void A_M2_app_purge_notice_names_its_scope_even_with_no_row()
    {
        var notices = new List<PurgeNotice>();
        _t.Store.Purged += notices.Add;
        _t.Store.Purge(PurgeScope.ForApp("notepad"));
        var n = Assert.Single(notices);
        Assert.Equal("app", n.Scope);
        Assert.Empty(n.Ids);
        _t.Store.Purge(PurgeScope.ForId("s_nope"));
        Assert.Single(notices);   // an id purge that deleted nothing still says nothing
    }

    [Fact]
    public void A_M2_app_fence_expires_like_the_time_fences()
    {
        long mono = 0;
        _t.Store.MonoMs = () => mono;
        var before = DateTimeOffset.UtcNow.AddSeconds(-5);
        _t.Store.Purge(PurgeScope.ForApp("notepad"));
        mono += (long)EncryptedStore.FenceLifetime.TotalMilliseconds;
        Assert.NotNull(_t.Store.AddScreenCapture(Caps.Screen(before, "notepad.exe")));
    }

    // ---- A-L6: a speech segment that runs into a purged range is refused

    [Fact]
    public void A_L6_audio_row_overlapping_a_purged_range_is_refused()
    {
        var now = DateTimeOffset.UtcNow;
        var from = now.AddSeconds(-5);
        _t.Store.Purge(PurgeScope.Range(from, now.AddMinutes(1)));
        // 4.2 s segment starting 0.6 s before the window: words from inside it.
        Assert.Throws<PurgedWindowException>(() => _t.Store.AddAudioCapture(Caps.Audio(from.AddMilliseconds(-600), "spoken across the start")));
        // A segment that ended before the window is kept.
        Assert.NotNull(_t.Store.AddAudioCapture(Caps.Audio(from.AddSeconds(-10), "before the window")));
    }

    // ---- C7: purge last and panic are open-ended upward

    [Fact]
    public void C7_purge_last_deletes_rows_stamped_in_the_future()
    {
        var future = Screen(_t.Store, DateTimeOffset.UtcNow.AddHours(2), seed: 9);   // captured before the clock stepped back 2 h
        var old = Screen(_t.Store, DateTimeOffset.UtcNow.AddHours(-1), seed: 10);
        Assert.Equal(1, _t.Store.CountPurge(PurgeScope.Last(TimeSpan.FromMinutes(5))));
        Assert.Equal(1, _t.Store.Purge(PurgeScope.Last(TimeSpan.FromMinutes(5))));
        Assert.Null(_t.Store.Get(future));
        Assert.NotNull(_t.Store.Get(old));
        // The fence still ends at now: new captures are not refused.
        Assert.NotNull(_t.Store.AddScreenCapture(Caps.Screen(DateTimeOffset.UtcNow.AddSeconds(1))));
    }

    [Fact]
    public void C7_panic_with_zero_minutes_still_deletes_nothing()
    {
        var future = Screen(_t.Store, DateTimeOffset.UtcNow.AddHours(2), seed: 11);
        Assert.Equal(0, _t.Store.Purge(PurgeScope.Last(TimeSpan.Zero)));
        Assert.NotNull(_t.Store.Get(future));
    }

    // ---- C3: export partials left by a killed agent are deleted at the next start

    [Fact]
    public async Task C3_leftover_export_partial_is_deleted_at_host_start()
    {
        using var h = new TempHome("dpapi");
        var left = Path.Combine(_out.Path, "out.zip.partial");
        var finished = Path.Combine(_out.Path, "done.zip");
        File.WriteAllBytes(left, [1, 2, 3]);
        File.WriteAllBytes(finished, [4]);
        ExportJournal.Add(h.Path, left);
        File.AppendAllLines(ExportJournal.PathFor(h.Path), [finished]);   // never a finished export, even if listed
        var host = CoreHost.Create(new CoreHostOptions(h.Path, h.Path));
        await using (host)
        {
            Assert.False(File.Exists(left));
            Assert.True(File.Exists(finished));
            Assert.False(File.Exists(ExportJournal.PathFor(h.Path)));
        }
    }

    [Fact]
    public void C3_capture_export_lists_its_partial_while_it_runs()
    {
        Screen(_t.Store, DateTimeOffset.UtcNow.AddMinutes(-4), seed: 12);
        var path = Path.Combine(_out.Path, "listed.zip");
        string[]? listed = null;
        ThumbData Transcode(ThumbData kf, string target)
        {
            listed = File.ReadAllLines(ExportJournal.PathFor(_t.Home.Path));
            return kf;
        }
        CaptureExporter.Write(_t.Store, new ExportRequest(DateTimeOffset.UtcNow.AddHours(-1), DateTimeOffset.UtcNow, path, Jpeg: true), Transcode, CancellationToken.None);
        Assert.Equal([Path.GetFullPath(path + ".partial")], listed!);
        Assert.True(File.Exists(path));
        Assert.False(File.Exists(ExportJournal.PathFor(_t.Home.Path)));   // done: nothing left to sweep
    }

    [Fact]
    public void C3_failed_export_leaves_a_file_it_did_not_create()
    {
        Screen(_t.Store, DateTimeOffset.UtcNow.AddMinutes(-4), seed: 13);
        var path = Path.Combine(_out.Path, "users.zip");
        var theirs = path + ".partial";
        File.WriteAllBytes(theirs, [9, 9]);
        using (new FileStream(theirs, FileMode.Open, FileAccess.Read, FileShare.Read))   // our partial cannot be created
            Assert.ThrowsAny<IOException>(() => CaptureExporter.Write(_t.Store,
                new ExportRequest(DateTimeOffset.UtcNow.AddHours(-1), DateTimeOffset.UtcNow, path, Jpeg: true), (kf, _) => kf, CancellationToken.None));
        Assert.True(File.Exists(theirs));
        Assert.False(File.Exists(ExportJournal.PathFor(_t.Home.Path)));   // not listed: the next start must not delete it
    }

    // ---- review #4: the old store is closed before the agent hears purge all

    [Fact]
    public async Task Purge_all_closes_the_old_store_before_the_agent_hears_it()
    {
        using var h = new TempHome("dpapi");
        var host = (CoreHostImpl)CoreHost.Create(new CoreHostOptions(h.Path, h.Path));
        await using (host)
        {
            await host.OpenStoreAsync(CancellationToken.None);
            var old = host.Store!;
            old.AddScreenCapture(Caps.Screen(DateTimeOffset.UtcNow.AddSeconds(-5)));   // a pending embedding job
            Exception? seen = null;
            ((ICoreHost)host).Purged += n =>
            {
                // The embedding scheduler still holding the old reference: no pre-purge image may come back after Clear().
                if (n.Reason == "all") seen = Record.Exception(() => old.TakePendingEmbeddings(10));
            };
            await host.PurgeAllAsync(CancellationToken.None);
            Assert.IsType<ObjectDisposedException>(seen);
        }
    }

    // ---- review #6: a replay file held open is reported and deleted at the next start

    [Fact]
    public async Task Purge_all_reports_a_replay_file_it_could_not_delete_and_the_next_start_deletes_it()
    {
        using var h = new TempHome("dpapi");
        var clip = Path.Combine(DataPaths.ReplaysDir(h.Path), "20261117T120000000Z.mp4");
        var host = (CoreHostImpl)CoreHost.Create(new CoreHostOptions(h.Path, h.Path));
        await using (host)
        {
            await host.OpenStoreAsync(CancellationToken.None);
            Directory.CreateDirectory(Path.GetDirectoryName(clip)!);
            File.WriteAllBytes(clip, [1, 2, 3]);   // a plain clip
            var p = new CommandProcessor(new CommandDeps
            {
                Settings = host.Settings,
                Store = () => host.Store,
                Keys = new FakeKeys(),
                Agent = new FakeAgent(),
                Embedder = new FakeEmbedder(),
                Thumbnailer = new FakeThumbs(),
                PurgeAll = host.PurgeAllAsync,
                PurgeAllUndeleted = () => host.LastPurgeAllUndeleted,
            });
            var ui = new ClientSession(PipeKind.Ui, 101, @"C:\app\ui\BetterRecall.UI.exe");
            ui.PurgeTokens["t1"] = new PurgeTicket(null, 0, DateTimeOffset.UtcNow.AddSeconds(30));
            Reply reply;
            using (new FileStream(clip, FileMode.Open, FileAccess.Read, FileShare.Read))   // a media player has it open
                reply = await p.HandleAsync(ui, "purge confirm t1", CancellationToken.None);
            var (ok, d, _) = Envelope.Parse(reply.Line);
            Assert.True(ok);
            Assert.Equal(1, d.GetProperty("undeleted").GetInt32());
            Assert.Equal(1, host.LastPurgeAllUndeleted);
            Assert.Contains(Path.GetFullPath(clip), File.ReadAllLines(ExportJournal.PathFor(h.Path)));
        }
        var next = CoreHost.Create(new CoreHostOptions(h.Path, h.Path));
        await using (next) Assert.False(File.Exists(clip));
    }

    [Fact]
    public void Purge_result_omits_undeleted_when_nothing_is_left()
    {
        Assert.Equal("{\"purged\":3}", JsonSerializer.Serialize(new PurgeResult(3), CoreJson.Default.PurgeResult));
    }
}
