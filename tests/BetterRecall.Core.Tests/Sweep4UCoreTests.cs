// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Text;
using System.Text.Json;
using BetterRecall.Contracts;
using BetterRecall.Core.Crypto;
using BetterRecall.Core.Ipc;

namespace BetterRecall.Core.Tests;

/// <summary>Bug sweep 4, batch U (control plane): Core side. Real CommandProcessor, real store, settings and key vault.</summary>
public sealed class Sweep4UCoreTests : IDisposable
{
    private readonly TempStore _t = new();
    private readonly TempHome _home = new();   // dpapi
    private readonly SettingsStore _settings;
    private readonly FakeAgent _agent = new();
    private readonly CommandProcessor _p;
    private readonly ClientSession _ui = new(PipeKind.Ui, 101, @"C:\app\ui\BetterRecall.UI.exe");
    private readonly ClientSession _ipc = new(PipeKind.Public, 202, @"C:\tools\script.exe");
    private IQueryEmbedder _embedder = new FakeEmbedder();

    public Sweep4UCoreTests()
    {
        _settings = SettingsStore.Load(_home.Path);
        Set("{\"ipc\":{\"enabled\":true,\"allowControl\":true,\"allowPurge\":true}}");
        _p = new CommandProcessor(new CommandDeps
        {
            Settings = _settings, Store = () => _t.Store, Keys = new FakeKeys(), Agent = _agent,
            Embedder = new DelegatingEmbedder(() => _embedder), Thumbnailer = new FakeThumbs(), PurgeAll = _ => Task.FromResult(0),
        });
        _t.Store.Purged += _p.OnStorePurged;   // as CoreHost wires every store
    }

    public void Dispose()
    {
        _t.Dispose();
        _home.Dispose();
    }

    private void Set(string json) => Assert.Null(_settings.TryUpdate(JsonDocument.Parse(json).RootElement));

    private static async Task<(bool Ok, JsonElement Data, string? Error)> Call(CommandProcessor p, ClientSession s, string line)
    {
        var r = await p.HandleAsync(s, line, CancellationToken.None);
        return Envelope.Parse(r.Line);
    }

    private Task<(bool Ok, JsonElement Data, string? Error)> Call(string line, ClientSession? s = null) => Call(_p, s ?? _ui, line);

    private static CommandProcessor Processor(SettingsStore st, IKeyVault keys, FakeAgent agent, UnlockPassword? pw = null) => new(new CommandDeps
    {
        Settings = st, Store = () => null, Keys = keys, Agent = agent, Embedder = new FakeEmbedder(), Thumbnailer = new FakeThumbs(),
        PurgeAll = _ => Task.FromResult(0), Password = pw,
    });

    private sealed class DelegatingEmbedder(Func<IQueryEmbedder> inner) : IQueryEmbedder
    {
        public bool Warm => inner().Warm;
        public Task<float[]> EmbedQueryAsync(string text, CancellationToken ct) => inner().EmbedQueryAsync(text, ct);
    }

    /// <summary>Query embedder that answers when the test says so (the 3-15 s worker wait of B9).</summary>
    private sealed class GatedEmbedder : IQueryEmbedder
    {
        public readonly TaskCompletionSource Entered = new(TaskCreationOptions.RunContinuationsAsynchronously);
        public readonly TaskCompletionSource Release = new(TaskCreationOptions.RunContinuationsAsynchronously);
        public bool Warm => true;
        public async Task<float[]> EmbedQueryAsync(string text, CancellationToken ct)
        {
            Entered.TrySetResult();
            await Release.Task.WaitAsync(ct);
            return Vec.Seeded(text.GetHashCode());
        }
    }

    private sealed class ThrowingEmbedder(Exception ex, bool warm) : IQueryEmbedder
    {
        public bool Warm => warm;
        public Task<float[]> EmbedQueryAsync(string text, CancellationToken ct) => Task.FromException<float[]>(ex);
    }

    /// <summary>Stream whose read side stays open until End (the subscriber holds the pipe).</summary>
    private sealed class HoldStream : Stream
    {
        private readonly TaskCompletionSource<int> _eof = new(TaskCreationOptions.RunContinuationsAsynchronously);
        private readonly StringBuilder _written = new();
        public string Written { get { lock (_written) return _written.ToString(); } }
        public void End() => _eof.TrySetResult(0);
        public override bool CanRead => true; public override bool CanSeek => false; public override bool CanWrite => true;
        public override long Length => 0; public override long Position { get => 0; set { } }
        public override void Flush() { }
        public override Task FlushAsync(CancellationToken ct) => Task.CompletedTask;
        public override int Read(byte[] b, int o, int c) => _eof.Task.Result;
        public override async ValueTask<int> ReadAsync(Memory<byte> m, CancellationToken ct = default) => await _eof.Task.WaitAsync(ct);
        public override Task<int> ReadAsync(byte[] b, int o, int c, CancellationToken ct) => _eof.Task.WaitAsync(ct);
        public override long Seek(long o, SeekOrigin s) => 0; public override void SetLength(long v) { }
        public override void Write(byte[] b, int o, int c) { lock (_written) _written.Append(Encoding.UTF8.GetString(b, o, c)); }
        public override ValueTask WriteAsync(ReadOnlyMemory<byte> m, CancellationToken ct = default) { lock (_written) _written.Append(Encoding.UTF8.GetString(m.Span)); return ValueTask.CompletedTask; }
        public override Task WriteAsync(byte[] b, int o, int c, CancellationToken ct) { Write(b, o, c); return Task.CompletedTask; }
    }

    private static async Task WaitFor(Func<bool> cond, int ms = 5000)
    {
        for (int i = 0; i < ms / 20 && !cond(); i++) await Task.Delay(20);
    }

    // ---- B1 + B6: a passphrase-wrapped key under keyProtection auto

    [Fact]
    public async Task B1_B6_passphrase_key_under_auto_status_asks_for_the_passphrase_and_says_locked()
    {
        using var h = new TempHome("passphrase");
        var s = SettingsStore.Load(h.Path);
        using (var first = new KeyVault(h.Path, () => s.Current, probeHello: false))
            Assert.True(first.UnlockWithPassphrase("correct horse battery"));   // creates the key, passphrase-wrapped
        // Later: Key protection = Auto saved (D3: auto keeps the passphrase-wrapped key), then an agent restart.
        Assert.Null(s.TryUpdate(JsonDocument.Parse("{\"security\":{\"keyProtection\":\"auto\",\"requireUnlock\":false}}").RootElement));
        using var vault = new KeyVault(h.Path, () => s.Current, probeHello: false);
        Assert.False(vault.TryLoadSilent());
        var agent = new FakeAgent { Unlocked = true };   // requireUnlock off: the read gate is open (B6)
        var p = Processor(s, vault, agent);

        var st = await Call(p, _ui, "status");
        Assert.True(st.Ok, st.Error);
        Assert.True(st.Data.GetProperty("locked").GetBoolean());                      // B6: agrees with "Store is locked."
        Assert.Equal("passphrase", st.Data.GetProperty("unlockNeeds").GetString());   // B1: not Hello, whatever settings say
        Assert.Equal("Store is locked.", (await Call(p, _ui, "timeline 2020-01-01T00:00:00Z 2030-01-01T00:00:00Z")).Error);
        Assert.True((await Call(p, _ipc, "status")).Data.GetProperty("locked").GetBoolean());   // MCP and scripts too

        // An empty unlock (the old Hello button) opens no data: still locked, still asking for the passphrase.
        Assert.True((await Call(p, _ui, "unlock")).Ok);
        st = await Call(p, _ui, "status");
        Assert.True(st.Data.GetProperty("locked").GetBoolean());
        Assert.Equal("passphrase", st.Data.GetProperty("unlockNeeds").GetString());

        Assert.True((await Call(p, _ui, "unlock correct horse battery")).Ok);
        Assert.True(vault.KeyLoaded);
        st = await Call(p, _ui, "status");
        Assert.False(st.Data.GetProperty("locked").GetBoolean());
        Assert.Equal("none", st.Data.GetProperty("unlockNeeds").GetString());
    }

    [Fact]
    public async Task B1_unlock_needs_follows_method_password_file_and_hello()
    {
        using var h = new TempHome();
        var s = SettingsStore.Load(h.Path);
        using var vault = new KeyVault(h.Path, () => s.Current, probeHello: false);
        Assert.True(vault.TryLoadSilent());
        var pw = new UnlockPassword(h.Path, memKiB: 1024, passes: 1, lanes: 1);
        var agent = new FakeAgent { Unlocked = false };
        var p = Processor(s, vault, agent, pw);
        async Task<string?> Needs() => (await Call(p, _ui, "status")).Data.GetProperty("unlockNeeds").GetString();

        Assert.Equal("hello", await Needs());
        // B13: hello method, no Windows Hello on this PC, an unlock password set: the agent takes the password.
        vault.SetHelloAvailableForTest(false);
        Assert.Equal("hello", await Needs());   // no password yet: the Hello button answers "No unlock password is set."
        pw.Set("secret-pw");
        Assert.Equal("password", await Needs());
        Assert.True((await Call(p, _ui, "unlock secret-pw")).Ok);   // and accepts it
        Assert.Equal("none", await Needs());
        agent.Unlocked = false;
        vault.SetHelloAvailableForTest(true);
        Set(s, "{\"security\":{\"unlockMethod\":\"password\"}}");
        Assert.Equal("password", await Needs());
        Set(s, "{\"security\":{\"unlockMethod\":\"hello\",\"keyProtection\":\"passphrase\"}}");
        Assert.Equal("passphrase", await Needs());   // passphrase checked (or the key re-wrapped) before Hello
        // Scripts never see it.
        Assert.False((await Call(p, _ipc, "status")).Data.TryGetProperty("unlockNeeds", out _));
    }

    private static void Set(SettingsStore s, string json) => Assert.Null(s.TryUpdate(JsonDocument.Parse(json).RootElement));

    // ---- B7: the heartbeat goes through StatusFor

    [Fact]
    public async Task B7_public_heartbeat_hides_store_and_queue_while_locked()
    {
        _agent.Status = _agent.Status with { Store = new StoreStats(42, 4242, 7, DateTimeOffset.UtcNow.AddDays(-1), DateTimeOffset.UtcNow, 3, 39), Queue = new QueueState(5) };
        _agent.Unlocked = false;   // ipc.requireUnlocked defaults on: the public gate is closed
        Assert.True(_p.IsLocked(PipeKind.Public));
        _p.StatusHeartbeat = TimeSpan.FromMilliseconds(200);
        var s = new HoldStream();
        var run = _p.StreamAsync(s, Subscription.Status, Envelope.Ok("{\"subscribed\":true}"), CancellationToken.None, PipeKind.Public);
        await WaitFor(() => s.Written.Split('\n').Length >= 4);   // ack plus a few heartbeats, no push at all
        _agent.RaiseStatus();
        _agent.RaiseStatus();   // the same locked status again: not sent twice
        await Task.Delay(100);
        s.End();
        await run.WaitAsync(TimeSpan.FromSeconds(10));
        var lines = s.Written.Split('\n', StringSplitOptions.RemoveEmptyEntries).Skip(1).ToList();
        Assert.True(lines.Count >= 2, s.Written);
        foreach (var l in lines)
        {
            var d = Envelope.Parse(l).Data;
            Assert.Equal(0, d.GetProperty("store").GetProperty("rows").GetInt64());
            Assert.Equal(0, d.GetProperty("queue").GetProperty("pending").GetInt32());
        }
        Assert.DoesNotContain("4242", s.Written);
    }

    // ---- B9: a lock during the query-embed wait stops the answer

    [Fact]
    public async Task B9_lock_during_the_embed_wait_answers_locked()
    {
        _t.Store.AddScreenCapture(Caps.Screen(DateTimeOffset.UtcNow.AddMinutes(-2), "notepad.exe", "secret plans"));
        var gated = new GatedEmbedder();
        _embedder = gated;
        var search = Call("search secret", _ipc);
        await gated.Entered.Task.WaitAsync(TimeSpan.FromSeconds(5));
        _agent.Unlocked = false;   // hotkey, tray or session lock while the worker embeds the query
        gated.Release.TrySetResult();
        var r = await search;
        Assert.False(r.Ok);
        Assert.Equal("Store is locked.", r.Error);
    }

    // ---- T1: vector mode without a query vector

    [Fact]
    public async Task T1_vector_search_degrades_instead_of_internal_error()
    {
        _embedder = new ThrowingEmbedder(new InvalidOperationException("Model not installed"), warm: true);
        var r = await Call("search {\"q\":\"invoice\",\"mode\":\"vector\"}", _ipc);
        Assert.True(r.Ok, r.Error);
        Assert.Equal(0, r.Data.GetProperty("count").GetInt32());
        Assert.Equal("vector-unavailable", r.Data.GetProperty("degraded").GetString());

        _embedder = new ThrowingEmbedder(new OperationCanceledException(), warm: false);   // cold worker, wait ran out
        r = await Call("search {\"q\":\"invoice\",\"mode\":\"vector\"}", _ipc);
        Assert.True(r.Ok, r.Error);
        Assert.Equal("vector", r.Data.GetProperty("degraded").GetString());
    }

    // ---- B10 + A-L5: removed events on the streams

    [Fact]
    public async Task B10_capture_and_status_streams_hear_purges_and_nsfw_drops()
    {
        var keep = _t.Store.AddScreenCapture(Caps.Screen(DateTimeOffset.UtcNow.AddMinutes(-3), "notepad.exe", "kept"));
        var gone = _t.Store.AddScreenCapture(Caps.Screen(DateTimeOffset.UtcNow.AddMinutes(-2), "bank.exe", "purged by the CLI"));
        var nsfw = _t.Store.AddScreenCapture(Caps.Screen(DateTimeOffset.UtcNow.AddMinutes(-1), "chrome.exe", "dropped later"));
        var captures = new HoldStream();
        var status = new HoldStream();
        var runC = _p.StreamAsync(captures, Subscription.Captures, Envelope.Ok("{\"subscribed\":\"captures\"}"), CancellationToken.None, PipeKind.Ui);
        var runS = _p.StreamAsync(status, Subscription.Status, Envelope.Ok("{\"subscribed\":true}"), CancellationToken.None, PipeKind.Ui);
        await Task.Delay(200);

        // Another client (a script on the public pipe) purges one capture.
        var preview = await Call("purge id " + gone, _ipc);
        Assert.True(preview.Ok, preview.Error);
        Assert.True((await Call("purge confirm " + preview.Data.GetProperty("confirm").GetString(), _ipc)).Ok);
        // The NSFW gate drops a capture that was announced when it was stored (A-L5).
        _t.Store.DropCapture(nsfw, "nsfw");

        await WaitFor(() => captures.Written.Contains(nsfw) && status.Written.Contains(nsfw));
        captures.End(); status.End();
        await Task.WhenAll(runC, runS).WaitAsync(TimeSpan.FromSeconds(10));
        foreach (var w in new[] { captures.Written, status.Written })
        {
            var events = w.Split('\n', StringSplitOptions.RemoveEmptyEntries).Select(l => Envelope.Parse(l).Data)
                .Where(d => d.ValueKind == JsonValueKind.Object && d.TryGetProperty("event", out _)).ToList();
            Assert.Contains(events, e => e.GetProperty("reason").GetString() == "purge" && e.GetProperty("ids").EnumerateArray().Any(i => i.GetString() == gone));
            Assert.Contains(events, e => e.GetProperty("reason").GetString() == "drop" && e.GetProperty("ids").EnumerateArray().Any(i => i.GetString() == nsfw));
            Assert.DoesNotContain(keep, w);
        }
        Assert.Equal("Not found.", (await Call("get " + nsfw, _ipc)).Error);
    }

    [Fact]
    public async Task B10_removed_events_follow_the_subscribers_gate_and_cap_ids()
    {
        var id = _t.Store.AddScreenCapture(Caps.Screen(DateTimeOffset.UtcNow.AddMinutes(-2), "bank.exe", "x"));
        _agent.Unlocked = false;   // the public pipe's gate is closed (ipc.requireUnlocked)
        var s = new HoldStream();
        var run = _p.StreamAsync(s, Subscription.Status, Envelope.Ok("{\"subscribed\":true}"), CancellationToken.None, PipeKind.Public);
        await Task.Delay(200);
        _t.Store.DropCapture(id, "nsfw");
        await Task.Delay(300);
        s.End();
        await run.WaitAsync(TimeSpan.FromSeconds(10));
        Assert.DoesNotContain(id, s.Written);

        var many = new PurgeNotice("retention", [.. Enumerable.Range(0, CommandProcessor.MaxRemovedIds + 1).Select(i => "s_" + i)]);
        var e = CommandProcessor.RemovedEventFor(many);
        Assert.True(e.Truncated);
        Assert.Empty(e.Ids);
    }

    // ---- B3 (D4): no partial save while settings.json is unreadable

    [Fact]
    public async Task B3_unreadable_settings_refuse_saves_and_consent_until_reset()
    {
        using var h = new TempHome(null);
        var path = DataPaths.SettingsFile(h.Path);
        File.WriteAllText(path, "{\"capture\":{\"consented\":true},\"privacy\":{\"redactEmails\":true,\"denyProcesses\":[\"notepad\"]},\"retention\":{\"days\":7}} x");
        var s = SettingsStore.Load(h.Path);
        Assert.NotNull(s.LoadError);
        var agent = new FakeAgent();
        var p = Processor(s, new FakeKeys(), agent);

        var set = await Call(p, _ui, "settings set {\"ui\":{\"theme\":\"dark\"}}");
        Assert.Equal(SettingsStore.ErrUnreadable, set.Error);
        Assert.Equal(SettingsStore.ErrUnreadable, (await Call(p, _ui, "consent")).Error);
        Assert.DoesNotContain("consent", agent.Calls);
        Assert.EndsWith(" x", File.ReadAllText(path));   // the user's file is untouched
        Assert.Equal(SettingsStore.ErrUnreadable, s.TryUpdate(JsonDocument.Parse("{\"ui\":{\"theme\":\"dark\"}}").RootElement));

        var reset = await Call(p, _ui, "settings reset");
        Assert.True(reset.Ok, reset.Error);
        Assert.Null(s.LoadError);
        Assert.True(File.Exists(path + ".bad"));
        Assert.Contains("redactEmails", File.ReadAllText(path + ".bad"));
        Assert.Null(s.TryUpdate(JsonDocument.Parse("{\"ui\":{\"theme\":\"dark\"}}").RootElement));
        Assert.Equal("settings.json loaded; there is nothing to reset.", (await Call(p, _ui, "settings reset")).Error);
    }

    // ---- batch P follow-up: purge all during a replay save or export answers the purge message

    /// <summary>FakeAgent whose replay save and export wait for the test, then fail as batch P's purge fence makes them.</summary>
    private sealed class ReplayAgent(FakeAgent inner) : IAgentControl
    {
        public readonly TaskCompletionSource Entered = new(TaskCreationOptions.RunContinuationsAsynchronously);
        public readonly TaskCompletionSource<Exception> Fail = new(TaskCreationOptions.RunContinuationsAsynchronously);
        public AgentStatus GetStatus() => inner.GetStatus();
        public void Pause(TimeSpan? d, string r) => inner.Pause(d, r);
        public void Resume(string r) => inner.Resume(r);
        public bool IsUnlocked => inner.IsUnlocked;
        public Task<bool> UnlockAsync(CancellationToken ct) => inner.UnlockAsync(ct);
        public void Lock() => inner.Lock();
        public void Consent() => inner.Consent();
        public async Task<ReplayClip> SaveReplayAsync(CancellationToken ct) { Entered.TrySetResult(); throw await Fail.Task; }
        public IReadOnlyList<ReplayClip> ListReplays() => inner.ListReplays();
        public async Task ExportReplayAsync(string id, string path, CancellationToken ct) { Entered.TrySetResult(); throw await Fail.Task; }
        public void DeleteReplay(string id) => inner.DeleteReplay(id);
        public void Panic() => inner.Panic();
        public Task<BenchResult> BenchAsync(CancellationToken ct) => inner.BenchAsync(ct);
        public event Action<AgentStatus>? StatusChanged { add => inner.StatusChanged += value; remove => inner.StatusChanged -= value; }
        public event Action<CaptureMeta>? CaptureStored { add => inner.CaptureStored += value; remove => inner.CaptureStored -= value; }
    }

    [Theory]
    [InlineData("replay save", "discarded")]
    [InlineData(@"replay export r1 C:\out\clip.mp4", "cancelled")]
    public async Task P_purge_all_during_a_replay_save_or_export_answers_the_purge_message(string line, string how)
    {
        var agent = new ReplayAgent(_agent);
        var p = new CommandProcessor(new CommandDeps
        {
            Settings = _settings, Store = () => _t.Store, Keys = new FakeKeys(), Agent = agent,
            Embedder = new FakeEmbedder(), Thumbnailer = new FakeThumbs(), PurgeAll = _ => Task.FromResult(0),
        });
        var call = Call(p, _ui, line);
        await agent.Entered.Task.WaitAsync(TimeSpan.FromSeconds(5));
        p.OnStorePurged(new PurgeNotice("all", [], DateTimeOffset.MinValue, DateTimeOffset.UtcNow));
        // Batch P: a discarded save throws InvalidOperationException, a cancelled export FileNotFoundException.
        agent.Fail.SetResult(how == "discarded" ? new InvalidOperationException("Purge all ran during the replay save; the clip was discarded.")
            : new FileNotFoundException("Replay not found."));
        var r = await call;
        Assert.Equal(CommandProcessor.ErrPurgedDuring, r.Error);

        // Without a purge the same failures keep their own answers.
        var plain = new ReplayAgent(_agent);
        var p2 = new CommandProcessor(new CommandDeps
        {
            Settings = _settings, Store = () => _t.Store, Keys = new FakeKeys(), Agent = plain,
            Embedder = new FakeEmbedder(), Thumbnailer = new FakeThumbs(), PurgeAll = _ => Task.FromResult(0),
        });
        plain.Fail.SetResult(new FileNotFoundException("Replay not found."));
        Assert.Equal("Not found.", (await Call(p2, _ui, line)).Error);
    }

    // ---- C9: a save while another process reads settings.json

    [Fact]
    public void C9_save_succeeds_while_a_reader_holds_the_file()
    {
        using var h = new TempHome();
        var s = SettingsStore.Load(h.Path);
        var path = DataPaths.SettingsFile(h.Path);
        // A reader as the CLI and MCP now open it: the replace goes through at once.
        using (new FileStream(path, FileMode.Open, FileAccess.Read, FileShare.ReadWrite | FileShare.Delete))
            Assert.Null(s.TryUpdate(JsonDocument.Parse("{\"ui\":{\"theme\":\"dark\"}}").RootElement));
        Assert.Equal("dark", SettingsStore.ReadOnly(h.Path).Ui.Theme);
        // A reader that shares Read only (File.ReadAllText in another process) for a moment: the save waits it out.
        var held = new FileStream(path, FileMode.Open, FileAccess.Read, FileShare.Read);
        var release = Task.Run(async () => { await Task.Delay(150); held.Dispose(); });
        Assert.Null(s.TryUpdate(JsonDocument.Parse("{\"ui\":{\"theme\":\"light\"}}").RootElement));
        release.Wait();
        Assert.Equal("light", SettingsStore.ReadOnly(h.Path).Ui.Theme);
    }

    [Fact]
    public void C9_readonly_does_not_block_the_agents_replace()
    {
        using var h = new TempHome();
        var s = SettingsStore.Load(h.Path);
        var path = DataPaths.SettingsFile(h.Path);
        var stop = new CancellationTokenSource();
        var readers = Enumerable.Range(0, 4).Select(_ => Task.Run(() =>
        {
            while (!stop.IsCancellationRequested) SettingsStore.ReadOnly(h.Path);
        })).ToArray();
        try
        {
            for (int i = 0; i < 60; i++)
                Assert.Null(s.TryUpdate(JsonDocument.Parse("{\"ui\":{\"theme\":\"" + (i % 2 == 0 ? "dark" : "light") + "\"}}").RootElement));
        }
        finally
        {
            stop.Cancel();
            Task.WaitAll(readers);
        }
        Assert.True(File.Exists(path));
    }
}
