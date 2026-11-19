// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Reflection;
using BetterRecall.Agent.Host;
using BetterRecall.Agent.Replay;
using BetterRecall.Contracts;
using BetterRecall.Core;
using BetterRecall.Core.Store;

namespace BetterRecall.Agent.Tests;

/// <summary>Bug sweep 5, batch U: Agent regression tests for B1 (ReplayLibrary lock), B5 (keylocked pause/replay after purge all), and C4 (EndSession shutdown).</summary>
public sealed class Sweep5UAgentTests : IDisposable
{
    private readonly string _home;

    public Sweep5UAgentTests()
    {
        _home = Path.Combine(Path.GetTempPath(), "br-s5u-" + Guid.NewGuid().ToString("N")[..12]);
        Directory.CreateDirectory(_home);
        File.WriteAllText(DataPaths.SettingsFile(_home), "{\"security\":{\"keyProtection\":\"dpapi\"}}");
        if (!BetterRecall.Core.Log.Initialized) BetterRecall.Core.Log.Init(Path.Combine(Path.GetTempPath(), "br-s5u-logs"), "agent");
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

    private sealed class LockMidExportCipher(Action onSecondReadOpen) : IBlobCipher
    {
        private int _reads;
        public Stream OpenWrite(string path) => File.Create(path);
        public Stream OpenRead(string path)
        {
            if (Interlocked.Increment(ref _reads) == 2) return new FlippingStream(onSecondReadOpen);
            return File.OpenRead(path);
        }

        private sealed class FlippingStream(Action onEnd) : Stream
        {
            private int _step;
            public override bool CanRead => true;
            public override bool CanSeek => false;
            public override bool CanWrite => false;
            public override long Length => 8;
            public override long Position { get => 0; set => throw new NotSupportedException(); }
            public override void Flush() { }
            public override int Read(byte[] buffer, int offset, int count) => 0;
            public override ValueTask<int> ReadAsync(Memory<byte> buffer, CancellationToken ct = default)
            {
                if (_step++ == 0)
                {
                    buffer.Span[..8].Fill(0x42);
                    onEnd();   // lock flips after chunks start writing to .partial
                    return ValueTask.FromResult(8);
                }
                return ValueTask.FromResult(0);
            }
            public override long Seek(long offset, SeekOrigin origin) => throw new NotSupportedException();
            public override void SetLength(long value) => throw new NotSupportedException();
            public override void Write(byte[] buffer, int offset, int count) => throw new NotSupportedException();
        }
    }

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

    // ---- B1: ReplayLibrary aborts and cleans .partial when read gate locks ----

    [Fact]
    public async Task B1_replay_library_aborts_and_deletes_partial_when_gate_locks_mid_export()
    {
        var dir = DataPaths.ReplaysDir(_home);
        Directory.CreateDirectory(dir);
        var id = ReplayLibrary.NewId(DateTimeOffset.UtcNow);
        File.WriteAllBytes(Path.Combine(dir, id + ".mp4.enc"), [1, 2, 3, 4]);

        bool locked = false;
        var lib = new ReplayLibrary(_home, () => new Settings(), new LockMidExportCipher(() => locked = true))
        {
            Locked = () => locked,
        };
        var target = Path.Combine(_home, "out", "locked.mp4");
        await Assert.ThrowsAsync<InvalidOperationException>(() => lib.ExportAsync(id, target, CancellationToken.None));
        Assert.False(File.Exists(target));
        Assert.False(File.Exists(target + ".partial"));
    }

    // ---- B5: keylocked pause & replay pause while Store == null after purge all in passphrase mode ----

    [Fact]
    public void B5_sync_key_lock_pause_and_replay_pause_cover_passphrase_purge_all_window()
    {
        var pause = new PauseController();
        Assert.True(AgentHost.ShouldPauseReplay(pause, duringFullscreen: false, storeOpen: false, consented: true));
        Assert.False(AgentHost.ShouldPauseReplay(pause, duringFullscreen: false, storeOpen: true, consented: true));

        AgentHost.SyncKeyLockPause(pause, storeOpen: true, needsPassphrase: false);
        Assert.False(pause.Has("keylocked"));

        AgentHost.SyncKeyLockPause(pause, storeOpen: false, needsPassphrase: true);
        Assert.True(pause.Has("keylocked"));
        Assert.Equal("keylocked", pause.Current().Reason);
        Assert.True(AgentHost.ShouldPauseReplay(pause, duringFullscreen: false, storeOpen: true, consented: true));
    }

    [Fact]
    public void B5_on_purged_all_in_passphrase_mode_sets_keylocked_pause_and_clears_capturing()
    {
        var core = new FakeCoreHost();
        core.SettingsStore.Current.Security.KeyProtection = "passphrase";
        core.SettingsStore.Current.Capture.Consented = true;
        var agent = new AgentHost(core, new AgentOptions(_home, _home, false));
        typeof(AgentHost).GetField("_captureStarted", BindingFlags.NonPublic | BindingFlags.Instance)!.SetValue(agent, true);

        typeof(AgentHost).GetMethod("OnPurged", BindingFlags.NonPublic | BindingFlags.Instance)!
            .Invoke(agent, [new PurgeNotice("all", [], DateTimeOffset.MinValue, DateTimeOffset.UtcNow)]);

        var st = agent.GetStatus();
        Assert.True(st.Paused);
        Assert.Equal("keylocked", st.PauseReason);
        Assert.False(st.Capturing);
    }

    // ---- C4: EndSessionShutdown stops writers once, shares pipe stop across EndSession and Dispose, and checkpoints cleanly ----

    [Fact]
    public async Task C4_end_session_shutdown_dedupes_writer_and_pipe_stops_with_dispose()
    {
        var core = new CountingCoreHost();
        var agent = new AgentHost(core, new AgentOptions(_home, _home, false));
        agent.EndSessionShutdown();
        await agent.DisposeAsync();
        Assert.Equal(1, core.PipeStops);
    }

    [Fact]
    public async Task C4_end_session_cancels_maintenance_and_checkpoints_real_store()
    {
        await using var host = (CoreHostImpl)CoreHost.Create(new CoreHostOptions(_home, _home));
        await host.OpenStoreAsync(CancellationToken.None);
        await using var agent = new AgentHost(host, new AgentOptions(_home, _home, false));
        agent.EndSessionShutdown();
        Assert.True(host.EncryptedStore!.Checkpoint(TimeSpan.FromSeconds(5)));
    }
}
