// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Diagnostics;
using System.Reflection;
using System.Threading.Channels;
using BetterRecall.Agent.Audio;
using BetterRecall.Agent.Capture;
using BetterRecall.Agent.Host;
using BetterRecall.Agent.Privacy;
using BetterRecall.Agent.Replay;
using BetterRecall.Agent.Worker;
using BetterRecall.Contracts;
using BetterRecall.Core;
using BetterRecall.Core.Store;

namespace BetterRecall.Agent.Tests;

/// <summary>Bug sweep 4, batch P (purge and export fences), agent side. The seam tests run the agent's Transcriber and
/// KeyframePipeline against a real CoreHost and store, with the in-process fake worker on the real private pipe.</summary>
public sealed class Sweep4PAgentTests : IDisposable
{
    private readonly string _home;

    public Sweep4PAgentTests()
    {
        _home = Path.Combine(Path.GetTempPath(), "br-s4p-" + Guid.NewGuid().ToString("N")[..12]);
        Directory.CreateDirectory(_home);
        File.WriteAllText(DataPaths.SettingsFile(_home), "{\"security\":{\"keyProtection\":\"dpapi\"}}");
        // Core logs go to the shared temp log folder, not into this home (which is deleted after the test).
        if (!BetterRecall.Core.Log.Initialized) BetterRecall.Core.Log.Init(Path.Combine(Path.GetTempPath(), "br-s4p-logs"), "core");
    }

    public void Dispose()
    {
        Microsoft.Data.Sqlite.SqliteConnection.ClearAllPools();
        for (int i = 0; i < 5; i++)
        {
            try { if (Directory.Exists(_home)) Directory.Delete(_home, true); return; }
            catch (IOException) { Thread.Sleep(100); }
            catch (UnauthorizedAccessException) { Thread.Sleep(100); }
        }
    }

    private static Task<int> PurgeAll(CoreHostImpl host) =>
        (Task<int>)typeof(CoreHostImpl).GetMethod("PurgeAllAsync", BindingFlags.NonPublic | BindingFlags.Instance)!.Invoke(host, [CancellationToken.None])!;

    private sealed class NullCapture : ICaptureStream { public bool Ended => false; public void Dispose() { } }

    private static async Task Until(Func<bool> cond, string what)
    {
        for (int i = 0; i < 500 && !cond(); i++) await Task.Delay(20);
        Assert.True(cond(), what);
    }

    // ---- A-H1 (audio): a segment in the worker when purge all lands is not stored in the new store

    [Fact]
    public async Task A_H1_transcript_in_flight_during_purge_all_is_not_stored_in_the_new_store()
    {
        var host = (CoreHostImpl)CoreHost.Create(new CoreHostOptions(_home, _home));
        await using (host)
        {
            await host.OpenStoreAsync(CancellationToken.None);
            var entered = new ManualResetEventSlim();
            var release = new ManualResetEventSlim();
            var launcher = new FakeWorkerLauncher
            {
                Handler = (req, payload) =>
                {
                    if (req.Op != "transcribe") return null;
                    bool pre = payload.Length / 4 == 16000;
                    if (pre) { entered.Set(); release.Wait(TimeSpan.FromSeconds(30)); }
                    return (new WorkerResponse(req.Id, true, Transcript: new TranscribeResult(true, pre ? "pre purge marker alpha" : "after the purge", "en", [], 3)), []);
                },
            };
            await using var sup = new WorkerSupervisor(launcher, "x", () => 600);
            int stored = 0;
            var tr = new Transcriber(new WorkerClient(sup), () => host.Store, () => new Settings(), () => Interlocked.Increment(ref stored));
            var hub = new LoopbackHub(_ => new NullCapture());
            using var src = new TranscriptSource("system", hub, () => new Settings(), () => false, () => Guardrails.AudioVerdict.Allowed);
            src.NoteGuard(0, new Guardrails.AudioVerdict(Guardrails.AudioGate.Allowed, "teams"));   // R: segments queued directly, no packets
            var q = (Channel<SpeechSegment>)typeof(TranscriptSource).GetField("_queue", BindingFlags.NonPublic | BindingFlags.Instance)!.GetValue(src)!;
            long nowQpc() => OutputCapture.Qpc100ns(Stopwatch.GetTimestamp());
            q.Writer.TryWrite(new SpeechSegment(Enumerable.Repeat(0.2f, 16000).ToArray(), nowQpc() - 20_000_000, 1000));   // spoken 2 s ago
            using var cts = new CancellationTokenSource();
            var run = tr.RunAsync(src, cts.Token);
            Assert.True(entered.Wait(TimeSpan.FromSeconds(20)), "transcribe never reached the worker");

            await PurgeAll(host);
            release.Set();
            await Task.Delay(200);
            q.Writer.TryWrite(new SpeechSegment(Enumerable.Repeat(0.2f, 8000).ToArray(), nowQpc(), 500));   // after the purge
            await Until(() => Volatile.Read(ref stored) == 1, "the post-purge segment was not stored");

            var store = host.Store!;
            Assert.Equal(1, store.Stats().Rows);
            var row = Assert.Single(store.Timeline(DateTimeOffset.UtcNow.AddHours(-1), DateTimeOffset.UtcNow.AddMinutes(1), 10));
            Assert.Equal("after the purge", store.Get(row.Id)!.Text);
            cts.Cancel();
            try { await run; } catch (OperationCanceledException) { }
        }
    }

    // ---- A-H1 (keyframes): a frame in OCR when purge all lands is not stored in the new store

    private static unsafe KeyframeGrab Grab(int monitor, byte fill, DateTimeOffset ts)
    {
        var req = new KeyframeRequest(ts, "notepad.exe", @"C:\notepad.exe", "PREPURGEA1", "ctx", true, 64, 0, "");
        var g = KeyframeGrab.Alloc(monitor, req, 64, 32, 0);
        new Span<byte>(g.Pixels, 64 * 32 * 4).Fill(fill);
        return g;
    }

    [Fact]
    public async Task A_H1_keyframe_in_ocr_during_purge_all_is_not_stored_in_the_new_store()
    {
        var host = (CoreHostImpl)CoreHost.Create(new CoreHostOptions(_home, _home));
        await using (host)
        {
            await host.OpenStoreAsync(CancellationToken.None);
            var entered = new ManualResetEventSlim();
            var release = new ManualResetEventSlim();
            int ocr = 0;
            var launcher = new FakeWorkerLauncher
            {
                Handler = (req, _) =>
                {
                    if (req.Op != "ocr") return null;
                    if (Interlocked.Increment(ref ocr) == 1) { entered.Set(); release.Wait(TimeSpan.FromSeconds(30)); }
                    return (new WorkerResponse(req.Id, true, Ocr: new OcrResult([], "fake", 1)), []);
                },
            };
            await using var sup = new WorkerSupervisor(launcher, "x", () => 600);
            var s = new Settings();
            s.Keyframes.Format = "webp";
            var metas = new System.Collections.Concurrent.ConcurrentQueue<CaptureMeta>();
            await using var pipe = new KeyframePipeline(() => host.Store, () => s, metas.Enqueue, new WorkerKeyframeReader(new WorkerClient(sup)), new RedactionMasks(), TimeSpan.FromSeconds(20));
            pipe.Post(Grab(0, 200, DateTimeOffset.UtcNow));
            Assert.True(entered.Wait(TimeSpan.FromSeconds(20)), "OCR never reached the worker");

            await PurgeAll(host);
            release.Set();
            await Task.Delay(300);
            pipe.Post(Grab(1, 50, DateTimeOffset.UtcNow.AddMilliseconds(5)));   // another monitor, after the purge
            await Until(() => Interlocked.Read(ref pipe.Kept) >= 1, "the post-purge keyframe was not kept");

            Assert.Equal(1, host.EncryptedStore!.CountPurge(new PurgeScope("all")));   // every row, embedded or not
            Assert.Equal(1, Assert.Single(metas).Monitor);
        }
    }

    // ---- A-M2: an app or site purge reaches the replay ring

    private static readonly DateTimeOffset Now = new(2026, 11, 17, 12, 0, 0, TimeSpan.Zero);

    [Fact]
    public void A_M2_app_and_site_purges_reach_the_ring_however_old_their_rows()
    {
        Assert.True(AgentHost.ReachesRing(new PurgeNotice("purge", ["s_1"], Newest: Now.AddHours(-3), Scope: "app"), Now, 120));
        Assert.True(AgentHost.ReachesRing(new PurgeNotice("purge", [], Scope: "site"), Now, 120));
        // An id purge of an old row still does not.
        Assert.False(AgentHost.ReachesRing(new PurgeNotice("purge", ["s_1"], Newest: Now.AddHours(-3)), Now, 120));
    }

    // ---- C1, BC-S1: purge all discards a replay save in flight

    private sealed class FakeCipher : IBlobCipher
    {
        public Stream OpenWrite(string path) => new FileStream(path, FileMode.Create, FileAccess.Write, FileShare.None);
        public Stream OpenRead(string path) => File.OpenRead(path);
    }

    [Theory]
    [InlineData(true)]
    [InlineData(false)]
    public async Task C1_BC_S1_replay_save_during_purge_all_leaves_no_clip(bool encrypted)
    {
        var lib = new ReplayLibrary(_home, () => new Settings(), new FakeCipher());
        var path = Path.Combine(_home, "replays", "20261117T120000000Z" + (encrypted ? ".mp4.enc" : ".mp4"));
        Directory.CreateDirectory(Path.GetDirectoryName(path)!);
        int Save(Stream s)
        {
            s.Write([1, 2, 3, 4]);
            lib.OnPurgeAll();   // purge all lands while the clip is encoded
            return 1000;
        }
        await Assert.ThrowsAsync<InvalidOperationException>(() => lib.WriteClipAsync(path, encrypted, Save, CancellationToken.None));
        Assert.False(File.Exists(path));
        Assert.False(File.Exists(path + ".partial"));

        // A save that sees no purge is kept.
        Assert.Equal(1000, await lib.WriteClipAsync(path, encrypted, s => { s.Write([5]); return 1000; }, CancellationToken.None));
        Assert.True(File.Exists(path));
    }

    // ---- C1, C3: replay exports write a journalled .partial; purge all cancels a running one

    /// <summary>Source of a replay export that hands out a few bytes, then waits (a long copy).</summary>
    private sealed class SlowSource : Stream
    {
        public readonly ManualResetEventSlim Started = new();
        private int _reads;
        public override bool CanRead => true;
        public override bool CanSeek => false;
        public override bool CanWrite => false;
        public override long Length => throw new NotSupportedException();
        public override long Position { get => 0; set => throw new NotSupportedException(); }
        public override void Flush() { }
        public override int Read(byte[] buffer, int offset, int count) => 0;   // the mvhd probe: an empty file
        public override async ValueTask<int> ReadAsync(Memory<byte> buffer, CancellationToken ct = default)
        {
            if (_reads++ == 0) { buffer.Span[..8].Fill(7); return 8; }
            Started.Set();
            await Task.Delay(Timeout.Infinite, ct);
            return 0;
        }
        public override long Seek(long offset, SeekOrigin origin) => throw new NotSupportedException();
        public override void SetLength(long value) => throw new NotSupportedException();
        public override void Write(byte[] buffer, int offset, int count) => throw new NotSupportedException();
    }

    private sealed class SlowCipher : IBlobCipher
    {
        public SlowSource? Last;
        public Stream OpenWrite(string path) => throw new NotSupportedException();
        public Stream OpenRead(string path) => Last = new SlowSource();
    }

    private string NewClip()
    {
        var dir = DataPaths.ReplaysDir(_home);
        Directory.CreateDirectory(dir);
        var id = ReplayLibrary.NewId(DateTimeOffset.UtcNow);
        File.WriteAllBytes(Path.Combine(dir, id + ".mp4.enc"), [1, 2, 3]);
        return id;
    }

    [Fact]
    public async Task C1_purge_all_cancels_a_running_replay_export_and_deletes_the_copy()
    {
        var cipher = new SlowCipher();
        var lib = new ReplayLibrary(_home, () => new Settings(), cipher);
        var id = NewClip();
        var target = Path.Combine(_home, "out", "clip.mp4");
        var export = lib.ExportAsync(id, target, CancellationToken.None);
        await Until(() => cipher.Last?.Started.IsSet == true, "export never started copying");
        Assert.True(File.Exists(target + ".partial"));
        Assert.Contains(Path.GetFullPath(target + ".partial"), File.ReadAllLines(ExportJournal.PathFor(_home)));
        Assert.False(File.Exists(target));   // never the final name while copying (C3)

        lib.OnPurgeAll();
        await Assert.ThrowsAsync<FileNotFoundException>(() => export.WaitAsync(TimeSpan.FromSeconds(10)));
        Assert.False(File.Exists(target));
        Assert.False(File.Exists(target + ".partial"));
        Assert.False(File.Exists(ExportJournal.PathFor(_home)));
    }

    [Fact]
    public async Task C3_replay_export_cancelled_by_the_caller_deletes_its_partial_and_finished_export_is_renamed()
    {
        var cipher = new SlowCipher();
        var lib = new ReplayLibrary(_home, () => new Settings(), cipher);
        var id = NewClip();
        var target = Path.Combine(_home, "out", "cancelled.mp4");
        using var cts = new CancellationTokenSource();
        var export = lib.ExportAsync(id, target, cts.Token);
        await Until(() => cipher.Last?.Started.IsSet == true, "export never started copying");
        cts.Cancel();
        await Assert.ThrowsAnyAsync<OperationCanceledException>(() => export.WaitAsync(TimeSpan.FromSeconds(10)));
        Assert.False(File.Exists(target));
        Assert.False(File.Exists(target + ".partial"));

        var plain = new ReplayLibrary(_home, () => new Settings(), new FakeCipher());
        var done = Path.Combine(_home, "out", "done.mp4");
        await plain.ExportAsync(id, done, CancellationToken.None);
        Assert.Equal([1, 2, 3], File.ReadAllBytes(done));
        Assert.False(File.Exists(done + ".partial"));
        Assert.False(File.Exists(ExportJournal.PathFor(_home)));
    }

    // ---- review #7: purge all through the agent's real wiring (AgentHost.OnPurged) stops a running replay export

    /// <summary>Decrypting source that holds the clip file open (a delete fails while it is), then waits.</summary>
    private sealed class HoldingCipher : IBlobCipher
    {
        public HoldingSource? Last;
        public Stream OpenWrite(string path) => throw new NotSupportedException();
        public Stream OpenRead(string path) => Last = new HoldingSource(File.OpenRead(path));
    }

    private sealed class HoldingSource(FileStream file) : Stream
    {
        public readonly ManualResetEventSlim Started = new();
        private int _reads;
        public override bool CanRead => true;
        public override bool CanSeek => false;
        public override bool CanWrite => false;
        public override long Length => throw new NotSupportedException();
        public override long Position { get => 0; set => throw new NotSupportedException(); }
        public override void Flush() { }
        public override int Read(byte[] buffer, int offset, int count) => 0;
        public override async ValueTask<int> ReadAsync(Memory<byte> buffer, CancellationToken ct = default)
        {
            if (_reads++ == 0) { buffer.Span[..8].Fill(7); return 8; }
            Started.Set();
            await Task.Delay(Timeout.Infinite, ct);
            return 0;
        }
        public override long Seek(long offset, SeekOrigin origin) => throw new NotSupportedException();
        public override void SetLength(long value) => throw new NotSupportedException();
        public override void Write(byte[] buffer, int offset, int count) => throw new NotSupportedException();
        protected override void Dispose(bool disposing) { if (disposing) file.Dispose(); base.Dispose(disposing); }
    }

    private static void Wire(AgentHost agent, ICoreHost core, ReplayLibrary lib)
    {
        typeof(AgentHost).GetField("_replays", BindingFlags.NonPublic | BindingFlags.Instance)!.SetValue(agent, lib);
        core.Purged += typeof(AgentHost).GetMethod("OnPurged", BindingFlags.NonPublic | BindingFlags.Instance)!.CreateDelegate<Action<PurgeNotice>>(agent);
    }

    [Fact]
    public async Task C1_purge_all_through_the_agent_stops_a_replay_export_and_empties_replays()
    {
        var host = (CoreHostImpl)CoreHost.Create(new CoreHostOptions(_home, _home));
        await using (host)
        {
            await host.OpenStoreAsync(CancellationToken.None);
            var agent = new AgentHost(host, new AgentOptions(_home, _home, false));
            var cipher = new HoldingCipher();
            var lib = new ReplayLibrary(_home, () => new Settings(), cipher) { PurgingAll = () => host.PurgingAll };
            Wire(agent, host, lib);
            var id = NewClip();
            var target = Path.Combine(_home, "out", "seam.mp4");
            var export = lib.ExportAsync(id, target, CancellationToken.None);
            await Until(() => cipher.Last?.Started.IsSet == true, "export never started copying");

            await PurgeAll(host);
            await Assert.ThrowsAsync<FileNotFoundException>(() => export.WaitAsync(TimeSpan.FromSeconds(10)));
            Assert.False(File.Exists(target));
            Assert.False(File.Exists(target + ".partial"));
            Assert.Empty(Directory.GetFiles(DataPaths.ReplaysDir(_home)));   // the notice came before the delete loop
            Assert.Equal(0, host.LastPurgeAllUndeleted);
        }
    }

    // ---- review #1: no save or export starts while purge all runs

    [Fact]
    public async Task C1_save_and_export_are_refused_while_purge_all_runs()
    {
        bool purging = true;
        var lib = new ReplayLibrary(_home, () => new Settings(), new FakeCipher()) { PurgingAll = () => purging };
        var id = NewClip();
        var clip = Path.Combine(DataPaths.ReplaysDir(_home), "20261117T130000000Z.mp4.enc");
        await Assert.ThrowsAsync<InvalidOperationException>(() => lib.WriteClipAsync(clip, true, s => 1000, CancellationToken.None));
        Assert.False(File.Exists(clip));
        Assert.False(File.Exists(clip + ".partial"));
        var target = Path.Combine(_home, "out", "refused.mp4");
        await Assert.ThrowsAsync<FileNotFoundException>(() => lib.ExportAsync(id, target, CancellationToken.None));
        Assert.False(File.Exists(target));
        Assert.False(File.Exists(target + ".partial"));
        purging = false;
    }

    [Fact]
    public async Task C1_export_whose_source_opened_as_purge_all_began_writes_nothing()
    {
        bool purging = false;
        var cipher = new FlagCipher(() => purging = true);
        var lib = new ReplayLibrary(_home, () => new Settings(), cipher) { PurgingAll = () => purging };
        var id = NewClip();
        purging = false;   // flips when the clip is opened: by the duration probe in Find, then by the export itself
        var target = Path.Combine(_home, "out", "late.mp4");
        await Assert.ThrowsAsync<FileNotFoundException>(() => lib.ExportAsync(id, target, CancellationToken.None));
        Assert.False(File.Exists(target));
        Assert.False(File.Exists(target + ".partial"));
        Assert.False(File.Exists(ExportJournal.PathFor(_home)));
    }

    private sealed class FlagCipher(Action onOpen) : IBlobCipher
    {
        public Stream OpenWrite(string path) => throw new NotSupportedException();
        public Stream OpenRead(string path) { onOpen(); return new MemoryStream([1, 2, 3]); }
    }

    // ---- review #3: a purge that reaches the ring (app purge, panic) discards a save in flight

    [Fact]
    public async Task App_purge_and_panic_discard_a_replay_save_in_flight()
    {
        var core = new FakeCoreHost();
        var agent = new AgentHost(core, new AgentOptions(_home, _home, false));
        var lib = new ReplayLibrary(_home, () => new Settings(), new FakeCipher());
        typeof(AgentHost).GetField("_replays", BindingFlags.NonPublic | BindingFlags.Instance)!.SetValue(agent, lib);
        var onPurged = typeof(AgentHost).GetMethod("OnPurged", BindingFlags.NonPublic | BindingFlags.Instance)!;
        var dir = DataPaths.ReplaysDir(_home);
        Directory.CreateDirectory(dir);

        var app = Path.Combine(dir, "20261117T140000000Z.mp4");
        await Assert.ThrowsAsync<InvalidOperationException>(() => lib.WriteClipAsync(app, false, s =>
        {
            s.Write([1]);
            onPurged.Invoke(agent, [new PurgeNotice("purge", [], Scope: "app")]);   // purge app notepad while encoding
            return 1000;
        }, CancellationToken.None));
        Assert.False(File.Exists(app));

        var panic = Path.Combine(dir, "20261117T140001000Z.mp4");
        await Assert.ThrowsAsync<InvalidOperationException>(() => lib.WriteClipAsync(panic, false, s =>
        {
            s.Write([1]);
            agent.Panic();   // panic with privacy.panicPurgeMinutes 0 raises no notice
            return 1000;
        }, CancellationToken.None));
        Assert.False(File.Exists(panic));
        Assert.False(File.Exists(panic + ".partial"));
    }

    // ---- review #5: WM_ENDSESSION stops the pipes and quits

    private sealed class CountingCoreHost : ICoreHost
    {
        private readonly FakeCoreHost _inner = new();
        public int PipeStops;
        public ISettingsStore Settings => _inner.Settings;
        public IKeyVault Keys => _inner.Keys;
        public IBlobCipher Blobs => _inner.Blobs;
        public IStore? Store => null;
        public Task OpenStoreAsync(CancellationToken ct) => Task.CompletedTask;
        public void StartPipes(IAgentControl agent, IQueryEmbedder embedder, IThumbnailer thumbnailer) { }
        public Task StopPipesAsync() { Interlocked.Increment(ref PipeStops); return Task.CompletedTask; }
        public ValueTask DisposeAsync() => ValueTask.CompletedTask;
    }

    [Fact]
    public void C3_end_session_stops_the_pipes()
    {
        var core = new CountingCoreHost();
        var agent = new AgentHost(core, new AgentOptions(_home, _home, false));
        typeof(AgentHost).GetMethod("OnEndSession", BindingFlags.NonPublic | BindingFlags.Instance)!.Invoke(agent, []);
        Assert.Equal(1, core.PipeStops);
    }
}
