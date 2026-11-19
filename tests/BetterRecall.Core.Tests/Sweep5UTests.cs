// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.IO.Compression;
using System.Security.Cryptography;
using System.Text;
using System.Text.Json;
using System.Text.Json.Nodes;
using BetterRecall.Contracts;
using BetterRecall.Core.Crypto;
using BetterRecall.Core.Ipc;
using BetterRecall.Core.Store;
using BetterRecall.UI.Logic;
using CoreSettings = BetterRecall.Core.SettingsStore;
using CoreStore = BetterRecall.Core.Store.EncryptedStore;

namespace BetterRecall.Core.Tests;

/// <summary>Bug sweep 5, batch U: Core unit regression tests for B1, B2, B3, B4, B5 and C4.</summary>
public sealed class Sweep5UCoreTests : IDisposable
{
    private readonly TempStore _t = new();
    private readonly TempHome _home = new();   // dpapi
    private readonly CoreSettings _settings;
    private readonly FakeAgent _agent = new();

    public Sweep5UCoreTests()
    {
        _settings = CoreSettings.Load(_home.Path);
    }

    public void Dispose()
    {
        _t.Dispose();
        _home.Dispose();
    }

    private void Set(string json) => Assert.Null(_settings.TryUpdate(JsonDocument.Parse(json).RootElement));

    private static CommandProcessor Processor(CoreSettings st, IKeyVault keys, IAgentControl agent,
        Func<IStore?>? store = null, IThumbnailer? thumbs = null, UnlockPassword? pw = null, Func<CancellationToken, Task<int>>? purgeAll = null) => new(new CommandDeps
    {
        Settings = st,
        Store = store ?? (() => null),
        Keys = keys,
        Agent = agent,
        Embedder = new FakeEmbedder(),
        Thumbnailer = thumbs ?? new FakeThumbs(),
        PurgeAll = purgeAll ?? (_ => Task.FromResult(0)),
        Password = pw,
    });

    private static async Task<(bool Ok, JsonElement Data, string? Error)> Call(CommandProcessor p, ClientSession s, string line)
    {
        var r = await p.HandleAsync(s, line, CancellationToken.None);
        return Envelope.Parse(r.Line);
    }

    private sealed class MutableKeys : IKeyVault
    {
        public string Mode { get; set; } = KeyVault.ModeDpapi;
        public string Protection => Mode;
        public bool Hello { get; set; } = true;
        public bool HelloAvailable => Hello;
        public bool Loaded { get; set; } = true;
        public bool KeyLoaded => Loaded;
        public bool Needs { get; set; }
        public bool NeedsPassphrase => Needs;
        public string? ExpectedPassphrase { get; set; }
        public int RetryAfter { get; set; }
        public int PassphraseRetryAfterSeconds => RetryAfter;
        public bool Destroyed { get; private set; }

        public bool UnlockWithPassphrase(string passphrase)
        {
            if (ExpectedPassphrase == null || passphrase != ExpectedPassphrase) return false;
            Loaded = true;
            Needs = false;
            return true;
        }

        public Task<bool> VerifyUserAsync(nint hwnd, string reason, CancellationToken ct) => Task.FromResult(Hello);

        public void DestroyKey()
        {
            Destroyed = true;
            Loaded = false;
            if (Mode == KeyVault.ModePassphrase) Needs = true;
        }
    }

    private sealed class CallbackThumbs(Action<ThumbData, string> onTranscode) : IThumbnailer
    {
        public Task<ThumbData?> GetThumbAsync(string id, int width, CancellationToken ct) => Task.FromResult<ThumbData?>(null);
        public void ClearCache() { }
        public ThumbData Transcode(ThumbData keyframe, string target)
        {
            onTranscode(keyframe, target);
            return keyframe with { Mime = "image/jpeg", Bytes = [0xFF, 0xD8, 0xFF, 0xD9] };
        }
    }

    private sealed class HoldExportAgent(FakeAgent inner) : IAgentControl
    {
        public readonly TaskCompletionSource Entered = new(TaskCreationOptions.RunContinuationsAsynchronously);
        public bool Unlocked { get => inner.Unlocked; set => inner.Unlocked = value; }
        public AgentStatus GetStatus() => inner.GetStatus() with { Locked = !inner.Unlocked };
        public void Pause(TimeSpan? duration, string reason) => inner.Pause(duration, reason);
        public void Resume(string reason) => inner.Resume(reason);
        public bool IsUnlocked => inner.Unlocked;
        public Task<bool> UnlockAsync(CancellationToken ct) => inner.UnlockAsync(ct);
        public void Lock() { inner.Lock(); StatusChanged?.Invoke(GetStatus()); }
        public void OpenReadGate() => inner.OpenReadGate();
        public void Consent() => inner.Consent();
        public Task<ReplayClip> SaveReplayAsync(CancellationToken ct) => inner.SaveReplayAsync(ct);
        public IReadOnlyList<ReplayClip> ListReplays() => inner.ListReplays();
        public async Task ExportReplayAsync(string id, string path, CancellationToken ct)
        {
            Entered.TrySetResult();
            await Task.Delay(Timeout.Infinite, ct);
        }
        public void DeleteReplay(string id) => inner.DeleteReplay(id);
        public void Panic() => inner.Panic();
        public Task<BenchResult> BenchAsync(CancellationToken ct) => inner.BenchAsync(ct);
        public event Action<AgentStatus>? StatusChanged;
        public event Action<CaptureMeta>? CaptureStored { add { } remove { } }
        public void RaiseLockStatus() { inner.Unlocked = false; StatusChanged?.Invoke(GetStatus()); }
    }

    private sealed class BlockingVectorIndex(Action? onRemove = null) : IVectorIndex
    {
        public readonly ManualResetEventSlim UpsertEntered = new();
        public readonly ManualResetEventSlim UpsertRelease = new(true);
        public int Count => 0;
        public void Upsert(long rid, ReadOnlySpan<float> vector, long tsMs, int kind, string appLower, string site = "")
        {
            UpsertEntered.Set();
            UpsertRelease.Wait(TimeSpan.FromSeconds(10));
        }
        public void Remove(long rid) => onRemove?.Invoke();
        public void Clear() { }
        public IReadOnlyList<VectorCandidate> Scan(ReadOnlySpan<float> query, VectorFilter filter, int topN) => [];
        public int AppId(string appLower) => 0;
    }

    // ---- B1: read-gate lock aborts plaintext ZIP and replay exports ----

    [Fact]
    public void B1_capture_exporter_aborts_when_gate_locks_mid_export_and_deletes_partial()
    {
        var t0 = DateTimeOffset.UtcNow.AddMinutes(-10);
        _t.Store.AddScreenCapture(Caps.Screen(t0, "notepad.exe", "one"));
        _t.Store.AddScreenCapture(Caps.Screen(t0.AddSeconds(10), "notepad.exe", "two"));
        var outDir = Path.Combine(_home.Path, "exports");
        Directory.CreateDirectory(outDir);
        var zip = Path.Combine(outDir, "b1-lock.zip");
        var req = new ExportRequest(t0.AddMinutes(-1), t0.AddMinutes(5), zip);

        int checks = 0;
        bool locked = false;
        Assert.Throws<ExportLockedException>(() =>
            CaptureExporter.Write(_t.Store, req, (k, _) => k, isLocked: () =>
            {
                if (++checks >= 2) locked = true;
                return locked;
            }, CancellationToken.None));

        Assert.False(File.Exists(zip));
        Assert.False(File.Exists(zip + ".partial"));
    }

    [Fact]
    public async Task B1_command_processor_export_and_replay_export_abort_on_lock_and_panic()
    {
        var ui = new ClientSession(PipeKind.Ui, 1, @"C:\app\ui\BetterRecall.UI.exe");
        var t0 = DateTimeOffset.UtcNow.AddMinutes(-5);
        var capId = _t.Store.AddScreenCapture(Caps.Screen(t0, "notepad.exe", "secret"));
        _t.Store.SetEmbedding(capId, Vec.Seeded(1), Vec.Info);
        var outDir = Path.Combine(_home.Path, "out");
        Directory.CreateDirectory(outDir);
        var zip = Path.Combine(outDir, "cmd-lock.zip");

        var thumbs = new CallbackThumbs((_, _) =>
        {
            _agent.Unlocked = false;
            _agent.Status = _agent.Status with { Locked = true };
            _agent.RaiseStatus();
        });
        var p = Processor(_settings, new FakeKeys(), _agent, () => _t.Store, thumbs);
        var reqJson = JsonSerializer.Serialize(new ExportRequest(t0.AddMinutes(-1), t0.AddMinutes(1), zip, Jpeg: true), CoreJson.Default.ExportRequest);
        var res = await Call(p, ui, "export " + reqJson);
        Assert.False(res.Ok);
        Assert.Equal(CommandProcessor.ErrLocked, res.Error);
        Assert.False(File.Exists(zip));
        Assert.False(File.Exists(zip + ".partial"));

        // Lock verb cancels an active replay export (returns ErrLocked instead of hanging or completing).
        var replayAgent = new HoldExportAgent(new FakeAgent());
        var pReplay = Processor(_settings, new FakeKeys(), replayAgent, () => _t.Store);
        var mp4 = Path.Combine(outDir, "clip.mp4");
        var expTask = pReplay.HandleAsync(ui, $"replay export 20261119T000000000Z {mp4}", CancellationToken.None);
        await replayAgent.Entered.Task.WaitAsync(TimeSpan.FromSeconds(5));
        var lockedRes = await Call(pReplay, ui, "lock");
        Assert.True(lockedRes.Ok);
        var expReply = Envelope.Parse((await expTask.WaitAsync(TimeSpan.FromSeconds(5))).Line);
        Assert.False(expReply.Ok);
        Assert.Equal(CommandProcessor.ErrLocked, expReply.Error);
    }

    // ---- B2 + B5: forgotten passphrase Hello -> wipe all without opening gate ----

    [Fact]
    public async Task B2_forgot_passphrase_empty_unlock_authorizes_purge_all_only_and_fixes_keylost()
    {
        var ui = new ClientSession(PipeKind.Ui, 1, @"C:\app\ui\BetterRecall.UI.exe");
        Set("{\"security\":{\"keyProtection\":\"passphrase\",\"unlockMethod\":\"hello\"}}");
        var keys = new MutableKeys { Mode = KeyVault.ModePassphrase, Loaded = false, Needs = true, Hello = true };
        var agent = new FakeAgent { Unlocked = false, UnlockResult = true };
        int purged = 0;
        var p = Processor(_settings, keys, agent, purgeAll: _ => { Interlocked.Increment(ref purged); keys.DestroyKey(); return Task.FromResult(4); });

        // Purge all is rejected before verification; replay save refuses while KeyNeedsPassphrase.
        Assert.Equal(CommandProcessor.ErrUnlockRequired, (await Call(p, ui, "purge all")).Error);
        Assert.Equal(CommandProcessor.ErrLocked, (await Call(p, ui, "replay save")).Error);

        // D1: empty unlock verifies Hello for purge all, returns unlocked:false, keeps gate closed.
        var u = await Call(p, ui, "unlock");
        Assert.True(u.Ok, u.Error);
        Assert.False(u.Data.GetProperty("unlocked").GetBoolean());
        Assert.False(agent.Unlocked);
        Assert.Contains("unlock", agent.Calls);

        var st = await Call(p, ui, "status");
        Assert.True(st.Ok);
        Assert.True(st.Data.GetProperty("locked").GetBoolean());
        Assert.Equal("passphrase", st.Data.GetProperty("unlockNeeds").GetString());

        var confirm = await Call(p, ui, "purge all");
        Assert.True(confirm.Ok, confirm.Error);
        var tok = confirm.Data.GetProperty("confirm").GetString()!;
        var done = await Call(p, ui, $"purge confirm {tok}");
        Assert.True(done.Ok, done.Error);
        Assert.Equal(4, done.Data.GetProperty("purged").GetInt32());
        Assert.Equal(1, purged);

        // Secondary B2 variant: real KeyVault on keylost + keyProtection passphrase reports "hello", not "passphrase".
        using var lostHome = new TempHome("passphrase");
        var lostSt = CoreSettings.Load(lostHome.Path);
        File.WriteAllBytes(DataPaths.StoreDb(lostHome.Path), [1, 2, 3]);
        using var lostVault = new KeyVault(lostHome.Path, () => lostSt.Current, probeHello: false);
        lostVault.SetHelloAvailableForTest(true);
        Assert.Throws<KeyLostException>(() => lostVault.TryLoadSilent());
        Assert.False(lostVault.NeedsPassphrase);
        var pLost = Processor(lostSt, lostVault, new FakeAgent { Unlocked = false });
        var lostStatus = await Call(pLost, ui, "status");
        Assert.Equal("hello", lostStatus.Data.GetProperty("unlockNeeds").GetString());
    }

    [Fact]
    public async Task B2_no_hello_forgotten_passphrase_allows_purge_all_without_unlock_window()
    {
        var ui = new ClientSession(PipeKind.Ui, 1, @"C:\app\ui\BetterRecall.UI.exe");
        Set("{\"security\":{\"keyProtection\":\"passphrase\",\"unlockMethod\":\"hello\"}}");
        var keys = new MutableKeys { Mode = KeyVault.ModePassphrase, Loaded = false, Needs = true, Hello = false };
        var agent = new FakeAgent { Unlocked = false, UnlockResult = false };
        var now = DateTimeOffset.UtcNow;
        var p = new CommandProcessor(new CommandDeps
        {
            Settings = _settings,
            Store = () => null,
            Keys = keys,
            Agent = agent,
            Embedder = new FakeEmbedder(),
            Thumbnailer = new FakeThumbs(),
            PurgeAll = _ => Task.FromResult(2),
        })
        { Now = () => now };

        // Must still fail before empty unlock attempt (preserves KeyStateTests contract).
        Assert.Equal(CommandProcessor.ErrUnlockRequired, (await Call(p, ui, "purge all")).Error);

        // Empty unlock records the 60 s purge-only allowance when Hello is missing on a keylocked store.
        var u = await Call(p, ui, "unlock");
        Assert.False(u.Ok);
        Assert.Equal(CommandProcessor.ErrNoPassword, u.Error);
        Assert.False(agent.Unlocked);

        var confirm = await Call(p, ui, "purge all");
        Assert.True(confirm.Ok, confirm.Error);
        var tok = confirm.Data.GetProperty("confirm").GetString()!;
        Assert.Equal(2, (await Call(p, ui, $"purge confirm {tok}")).Data.GetProperty("purged").GetInt32());

        now = now.AddSeconds(61);
        Assert.Equal(CommandProcessor.ErrUnlockRequired, (await Call(p, ui, "purge all")).Error);
    }

    // ---- B3 (D2): refuse keyProtection=passphrase without Hello under unlockMethod=hello, and escape stuck setups ----

    [Fact]
    public async Task B3_hello_missing_refuses_passphrase_protection_and_unlocks_stuck_setup_with_password()
    {
        var ui = new ClientSession(PipeKind.Ui, 1, @"C:\app\ui\BetterRecall.UI.exe");
        using var vault = new KeyVault(_home.Path, () => _settings.Current, probeHello: false);
        vault.SetHelloAvailableForTest(false);
        Assert.True(vault.TryLoadSilent());
        Assert.Equal(KeyVault.ModeDpapi, vault.Protection);

        var pw = new UnlockPassword(_home.Path, memKiB: 1024, passes: 1, lanes: 1);
        pw.Set("pw1234");   // 6 chars (< 8-char passphrase rule)
        var agent = new FakeAgent { Unlocked = true, UnlockResult = false };
        var p = Processor(_settings, vault, agent, store: () => _t.Store, pw: pw);

        // D2: cannot set keyProtection: "passphrase" while HelloMissing && unlockMethod == "hello".
        var bad = await Call(p, ui, "settings set {\"security\":{\"keyProtection\":\"passphrase\"}}");
        Assert.False(bad.Ok);
        Assert.Equal(CommandProcessor.ErrPassphraseNeedsHello, bad.Error);

        // Already-stuck setup: settings.json has keyProtection:"passphrase" + unlockMethod:"hello" on DPAPI key, no Hello.
        Set("{\"security\":{\"keyProtection\":\"passphrase\",\"unlockMethod\":\"hello\",\"requireUnlock\":true}}");
        agent.Unlocked = false;
        var st = await Call(p, ui, "status");
        Assert.Equal("password", st.Data.GetProperty("unlockNeeds").GetString());

        // 6-char unlock password opens the gate without hitting ErrPassphraseRule or re-wrapping without Hello.
        var unlock = await Call(p, ui, "unlock pw1234");
        Assert.True(unlock.Ok, unlock.Error);
        Assert.True(unlock.Data.GetProperty("unlocked").GetBoolean());
        Assert.True(agent.Unlocked);
        Assert.Equal(KeyVault.ModeDpapi, vault.Protection);

        // Now user can cleanly recover settings off passphrase.
        var fixedSet = await Call(p, ui, "settings set {\"security\":{\"keyProtection\":\"auto\"}}");
        Assert.True(fixedSet.Ok, fixedSet.Error);
    }

    // ---- B4 (D3): password unlock on passphrase-wrapped key verifies passphrase inline ----

    [Fact]
    public async Task B4_password_unlock_verifies_passphrase_inline_without_relock_roundtrip()
    {
        var ui = new ClientSession(PipeKind.Ui, 1, @"C:\app\ui\BetterRecall.UI.exe");
        Set("{\"security\":{\"keyProtection\":\"passphrase\",\"unlockMethod\":\"password\",\"requireUnlock\":true}}");
        var keys = new MutableKeys { Mode = KeyVault.ModePassphrase, Loaded = true, Needs = false, ExpectedPassphrase = "data-passphrase-123" };
        var pw = new UnlockPassword(_home.Path, memKiB: 1024, passes: 1, lanes: 1);
        pw.Set("unlockpw");
        var agent = new FakeAgent { Unlocked = false };
        var p = Processor(_settings, keys, agent, store: () => _t.Store, pw: pw);

        Assert.True((await Call(p, ui, "unlock unlockpw")).Ok);
        Assert.True(agent.Unlocked);

        // Changing keyProtection off passphrase rejects until passphrase is verified inline.
        var blocked = await Call(p, ui, "settings set {\"security\":{\"keyProtection\":\"dpapi\"}}");
        Assert.Equal(CommandProcessor.ErrPassphraseNeeded, blocked.Error);

        // Wrong passphrase is rejected.
        var wrong = await Call(p, ui, "passphrase verify {\"passphrase\":\"wrong-pass\"}");
        Assert.False(wrong.Ok);
        Assert.Equal(CommandProcessor.ErrWrongPassphrase, wrong.Error);

        // Correct passphrase authorizes the protection change inline while staying unlocked.
        Assert.True((await Call(p, ui, "passphrase verify {\"passphrase\":\"data-passphrase-123\"}")).Ok);
        var saved = await Call(p, ui, "settings set {\"security\":{\"keyProtection\":\"dpapi\"}}");
        Assert.True(saved.Ok, saved.Error);
        Assert.True(agent.Unlocked);
    }

    // ---- C4: PipeManager parallel stop, CaptureExporter ct check in JSON/move, Store Checkpoint & CancelMaintenance ----

    [Fact]
    public async Task C4_pipe_manager_stops_all_three_pipes_and_core_host_dedupes_in_flight_stop()
    {
        Set("{\"ipc\":{\"enabled\":true},\"mcp\":{\"enabled\":true}}");
        var p = Processor(_settings, new FakeKeys(), _agent, () => _t.Store);
        var tag = Guid.NewGuid().ToString("N")[..10];
        var pm = new PipeManager(_settings, p, @"C:\NotTheInstallDir", () => _t.Store, (k, _) => $"BrSweep5U.{tag}.{k}");
        await pm.StartAsync();
        Assert.NotNull(pm.Ui);
        Assert.NotNull(pm.Public);
        Assert.NotNull(pm.Mcp);

        await pm.StopAsync();
        Assert.Null(pm.Ui);
        Assert.Null(pm.Public);
        Assert.Null(pm.Mcp);

        // CoreHostImpl.StopPipesAsync shares an active stop task cleanly across concurrent callers (EndSession + Dispose).
        var host = (CoreHostImpl)CoreHost.Create(new CoreHostOptions(_home.Path, Path.GetDirectoryName(Environment.ProcessPath)!));
        await using (host)
        {
            await host.OpenStoreAsync(CancellationToken.None);
            host.StartPipes(_agent, new FakeEmbedder(), new FakeThumbs());
            var t1 = host.StopPipesAsync();
            var t2 = host.StopPipesAsync();
            Assert.Same(t1, t2);
            await Task.WhenAll(t1, t2);
        }
    }

    [Fact]
    public void C4_capture_exporter_checks_cancellation_inside_captures_json_before_move()
    {
        var t0 = DateTimeOffset.UtcNow.AddMinutes(-2);
        var capId = _t.Store.AddScreenCapture(Caps.Screen(t0, "notepad.exe", "last capture"));
        _t.Store.SetEmbedding(capId, Vec.Seeded(1), Vec.Info);
        var outDir = Path.Combine(_home.Path, "exports-c4");
        Directory.CreateDirectory(outDir);
        var zip = Path.Combine(outDir, "c4-cancel.zip");
        var req = new ExportRequest(t0.AddMinutes(-1), t0.AddMinutes(1), zip, Jpeg: true);

        using var cts = new CancellationTokenSource();
        Assert.ThrowsAny<OperationCanceledException>(() =>
            CaptureExporter.Write(_t.Store, req, (kf, _) => { cts.Cancel(); return kf; }, isLocked: null, cts.Token));
        Assert.False(File.Exists(zip));
        Assert.False(File.Exists(zip + ".partial"));
    }

    [Fact]
    public async Task C4_encrypted_store_cancel_maintenance_and_bounded_checkpoint_do_not_deadlock()
    {
        using var home = new TempHome(null);
        var key = RandomNumberGenerator.GetBytes(32);
        CoreStore? storeRef = null;
        var index = new BlockingVectorIndex(onRemove: () => storeRef!.CancelMaintenance());
        using var store = new CoreStore(home.Path, () => (byte[])key.Clone(), index);
        storeRef = store;

        // 1) Bounded Checkpoint returns false cleanly within timeout while _wl is held, then succeeds when released.
        var id = store.AddScreenCapture(Caps.Screen(DateTimeOffset.UtcNow.AddDays(-60), "notepad.exe", "old"));
        index.UpsertRelease.Reset();
        var embedTask = Task.Run(() => store.SetEmbedding(id, Vec.Seeded(1), Vec.Info));
        Assert.True(index.UpsertEntered.Wait(TimeSpan.FromSeconds(5)));

        Assert.False(store.Checkpoint(TimeSpan.FromMilliseconds(40)));
        index.UpsertRelease.Set();
        await embedTask.WaitAsync(TimeSpan.FromSeconds(5));
        Assert.True(store.Checkpoint(TimeSpan.FromSeconds(5)));

        // 2) CancelMaintenance aborts remaining slices cleanly and still delivers retention notice for step 1.
        PurgeNotice? notice = null;
        store.Purged += n => notice = n;
        var settings = new Settings();
        settings.Retention.Days = 30;
        store.RunMaintenance(settings, retention: true, CancellationToken.None);
        Assert.NotNull(notice);
        Assert.Contains(id, notice!.Ids);
    }
}

/// <summary>Bug sweep 5, batch U seam tests: UI Logic (AgentApi, PurgeFlow, SettingsEditor, SettingsSaveFlow) against real CommandProcessor on a real UI pipe.</summary>
public sealed class Sweep5USeamTests
{
    private sealed class SeamMutableKeys(string protection = KeyVault.ModeDpapi, bool keyLoaded = true, bool needsPassphrase = false, bool hello = true, string? pass = null) : IKeyVault
    {
        public string Protection { get; set; } = protection;
        public bool HelloAvailable { get; set; } = hello;
        public bool KeyLoaded { get; set; } = keyLoaded;
        public bool NeedsPassphrase { get; set; } = needsPassphrase;
        public int DestroyCount { get; private set; }
        public bool UnlockWithPassphrase(string passphrase)
        {
            if (pass is null || passphrase != pass) return false;
            KeyLoaded = true;
            NeedsPassphrase = false;
            return true;
        }
        public Task<bool> VerifyUserAsync(nint hwnd, string reason, CancellationToken ct) => Task.FromResult(HelloAvailable);
        public void DestroyKey()
        {
            DestroyCount++;
            KeyLoaded = false;
            if (Protection == KeyVault.ModePassphrase) NeedsPassphrase = true;
        }
    }

    private sealed class BlockingTranscodeThumbs(Action onStart) : IThumbnailer
    {
        public Task<ThumbData?> GetThumbAsync(string id, int width, CancellationToken ct) => Task.FromResult<ThumbData?>(null);
        public void ClearCache() { }
        public ThumbData Transcode(ThumbData keyframe, string target)
        {
            onStart();
            return keyframe with { Mime = "image/jpeg", Bytes = [0xFF, 0xD8, 0xFF, 0xD9] };
        }
    }

    private sealed class RealPipeHarness : IAsyncDisposable
    {
        public readonly TempHome Home;
        public readonly CoreSettings Settings;
        public readonly CoreStore? Store;
        public readonly SeamAgent Agent = new();
        public readonly CommandProcessor Processor;
        public readonly PipeServer Server;
        private readonly IDisposable? _keysOwned;

        public RealPipeHarness(string? settingsJson, Func<string, CoreSettings, IKeyVault>? makeKeys = null, bool openStore = true,
            IThumbnailer? thumbs = null, UnlockPassword? pw = null, Func<CancellationToken, Task<int>>? purgeAll = null, IAgentControl? agent = null)
        {
            Home = new TempHome(null);
            if (settingsJson != null) File.WriteAllText(DataPaths.SettingsFile(Home.Path), settingsJson);
            Settings = CoreSettings.Load(Home.Path);
            var keys = makeKeys?.Invoke(Home.Path, Settings) ?? new SeamKeys();
            _keysOwned = keys as IDisposable;
            if (openStore)
            {
                var k = RandomNumberGenerator.GetBytes(32);
                Store = new CoreStore(Home.Path, () => (byte[])k.Clone());
            }
            Processor = new CommandProcessor(new CommandDeps
            {
                Settings = Settings,
                Store = () => Store,
                Keys = keys,
                Agent = agent ?? Agent,
                Embedder = new FakeEmbedder(),
                Thumbnailer = thumbs ?? new FakeThumbs(),
                PurgeAll = purgeAll ?? (_ => Task.FromResult(0)),
                Password = pw,
            });
            if (Store != null) Store.Purged += Processor.OnStorePurged;
            Server = new PipeServer("BetterRecall.sweep5u." + Guid.NewGuid().ToString("N"), PipeKind.Ui, _ => true, Processor.ServeAsync);
            Server.Start();
        }

        public AgentClient NewClient() => new(Server.Name, new ServerTrustVerifier(Environment.ProcessPath!));

        public async ValueTask DisposeAsync()
        {
            await Server.DisposeAsync();
            Store?.Dispose();
            _keysOwned?.Dispose();
            Home.Dispose();
        }
    }

    [Fact]
    public async Task Seam_B1_lock_mid_export_over_ui_pipe_returns_locked_and_leaves_no_zip()
    {
        RealPipeHarness? h = null;
        var thumbs = new BlockingTranscodeThumbs(() =>
        {
            h!.Agent.Lock();
            h.Agent.RaiseStatus();
        });
        h = new RealPipeHarness("{\"security\":{\"keyProtection\":\"dpapi\"}}", thumbs: thumbs);
        await using (h)
        {
            var t0 = DateTimeOffset.UtcNow.AddMinutes(-2);
            var capId = h.Store!.AddScreenCapture(Caps.Screen(t0, "notepad.exe", "confidential"));
            h.Store!.SetEmbedding(capId, Vec.Seeded(1), Vec.Info);
            var outDir = Path.Combine(h.Home.Path, "seam-out");
            Directory.CreateDirectory(outDir);
            var zip = Path.Combine(outDir, "seam.zip");

            await using var client = h.NewClient();
            var api = new AgentApi(client);
            var r = await api.ExportAsync(new ExportRequest(t0.AddMinutes(-1), t0.AddMinutes(1), zip, Jpeg: true));
            Assert.False(r.Ok);
            Assert.Equal(AgentErrors.StoreLocked, r.Error);
            Assert.False(File.Exists(zip));
            Assert.False(File.Exists(zip + ".partial"));
        }
    }

    [Fact]
    public async Task Seam_B2_forgot_passphrase_hello_runs_purge_all_and_keeps_gate_locked()
    {
        var keys = new SeamMutableKeys(KeyVault.ModePassphrase, keyLoaded: false, needsPassphrase: true, hello: true);
        int wiped = 0;
        await using var h = new RealPipeHarness("{\"security\":{\"keyProtection\":\"passphrase\",\"unlockMethod\":\"hello\"}}",
            (_, _) => keys, openStore: false, purgeAll: _ => { Interlocked.Increment(ref wiped); keys.DestroyKey(); return Task.FromResult(7); });
        h.Agent.Unlocked = false;

        await using var client = h.NewClient();
        var api = new AgentApi(client);
        var before = (await api.StatusAsync()).Value!;
        Assert.Equal(UnlockMode.Passphrase, UnlockLogic.Mode(before.UnlockNeeds, "passphrase", "hello", true));
        Assert.True(UnlockLogic.ShowForgotPassphrase(UnlockMode.Passphrase));

        var res = await PurgeFlow.PurgeAllAsync(api, passphrase: null);
        Assert.True(res.Ok, res.Error);
        Assert.Equal(7, res.Value!.Purged);
        Assert.Equal(1, wiped);
        Assert.Equal(1, keys.DestroyCount);

        // Read gate remained closed: status stays locked and search refuses.
        var after = (await api.StatusAsync()).Value!;
        Assert.True(after.Locked);
        Assert.False(h.Agent.Unlocked);
        Assert.Equal(AgentErrors.StoreLocked, (await api.SearchAsync(new SearchQuery("secret"))).Error);
    }

    [Fact]
    public async Task Seam_B2_no_hello_forgot_passphrase_wipes_via_purge_flow()
    {
        var keys = new SeamMutableKeys(KeyVault.ModePassphrase, keyLoaded: false, needsPassphrase: true, hello: false);
        int wiped = 0;
        var agent = new FakeAgent { Unlocked = false, UnlockResult = false };
        agent.Status = agent.Status with { Locked = true };
        using var tmpHome = new TempHome(null);
        File.WriteAllText(DataPaths.SettingsFile(tmpHome.Path), "{\"security\":{\"keyProtection\":\"passphrase\",\"unlockMethod\":\"hello\"}}");
        var settings = CoreSettings.Load(tmpHome.Path);
        var proc = new CommandProcessor(new CommandDeps
        {
            Settings = settings,
            Store = () => null,
            Keys = keys,
            Agent = agent,
            Embedder = new FakeEmbedder(),
            Thumbnailer = new FakeThumbs(),
            PurgeAll = _ => { Interlocked.Increment(ref wiped); keys.DestroyKey(); return Task.FromResult(3); },
        });
        await using var srv = new PipeServer("BetterRecall.sweep5u.noh." + Guid.NewGuid().ToString("N"), PipeKind.Ui, _ => true, proc.ServeAsync);
        srv.Start();
        await using var client = new AgentClient(srv.Name, new ServerTrustVerifier(Environment.ProcessPath!));
        var api = new AgentApi(client);

        var res = await PurgeFlow.PurgeAllAsync(api, passphrase: null);
        Assert.True(res.Ok, res.Error);
        Assert.Equal(3, res.Value!.Purged);
        Assert.Equal(1, wiped);
        Assert.False(agent.Unlocked);
    }

    [Fact]
    public async Task Seam_B3_hello_missing_blocks_passphrase_setting_and_escapes_stuck_setup()
    {
        using var tmpHome = new TempHome("dpapi");
        var pw = new UnlockPassword(tmpHome.Path, memKiB: 1024, passes: 1, lanes: 1);
        pw.Set("pw1234");
        var agent = new FakeAgent { Unlocked = true, UnlockResult = false };
        await using var h = new RealPipeHarness("{\"security\":{\"keyProtection\":\"dpapi\",\"unlockMethod\":\"hello\"}}",
            (home, st) =>
            {
                var v = new KeyVault(home, () => st.Current, probeHello: false);
                v.SetHelloAvailableForTest(false);
                Assert.True(v.TryLoadSilent());
                return v;
            }, openStore: true, pw: pw, agent: agent);

        await using var client = h.NewClient();
        var api = new AgentApi(client);
        var pws = (await api.PasswordStatusAsync()).Value!;
        Assert.False(pws.HelloAvailable);
        Assert.True(UnlockLogic.BlocksPassphraseChoice(pws.HelloAvailable, "hello"));

        var editor = new SettingsEditor();
        editor.Load((await api.SettingsGetAsync()).Value!);
        var blocked = await editor.SaveKeyAsync(api, "security.keyProtection", "passphrase");
        Assert.False(blocked.Ok);
        Assert.Equal(AgentErrors.PassphraseNeedsHello, blocked.Error);

        // Stuck setup (passphrase + hello in settings.json on DPAPI key without Hello): unlocks via 6-char password.
        Assert.Null(h.Settings.TryUpdate(JsonDocument.Parse("{\"security\":{\"keyProtection\":\"passphrase\",\"unlockMethod\":\"hello\"}}").RootElement));
        agent.Lock();
        agent.Status = agent.Status with { Locked = true };
        var st = (await api.StatusAsync()).Value!;
        Assert.Equal("password", st.UnlockNeeds);
        Assert.True((await api.UnlockAsync("pw1234")).Ok);
        Assert.True(agent.Unlocked);
    }

    [Fact]
    public async Task Seam_B4_settings_save_flow_verifies_passphrase_inline_after_password_unlock()
    {
        const string pass = "strong-data-passphrase";
        var keys = new SeamMutableKeys(KeyVault.ModePassphrase, keyLoaded: true, needsPassphrase: false, hello: true, pass: pass);
        using var pwHome = new TempHome(null);
        var pw = new UnlockPassword(pwHome.Path, memKiB: 1024, passes: 1, lanes: 1);
        pw.Set("unlockpw");
        await using var h = new RealPipeHarness("{\"security\":{\"keyProtection\":\"passphrase\",\"unlockMethod\":\"password\",\"requireUnlock\":true}}",
            (_, _) => keys, openStore: true, pw: pw);
        h.Agent.Unlocked = false;

        await using var client = h.NewClient();
        var api = new AgentApi(client);
        Assert.True((await api.UnlockAsync("unlockpw")).Ok);

        var editor = new SettingsEditor();
        editor.Load((await api.SettingsGetAsync()).Value!);
        editor.Set("security.keyProtection", "dpapi");

        int prompts = 0;
        var (outcome, _) = await SettingsSaveFlow.SaveWithPassphraseAsync(editor, api, async verify =>
        {
            prompts++;
            var bad = await verify("wrong");
            Assert.False(bad.Ok);
            var good = await verify(pass);
            Assert.True(good.Ok, good.Error);
            return good.Ok;
        });

        Assert.Equal(SaveOutcome.Saved, outcome);
        Assert.Equal(1, prompts);
        Assert.True(h.Agent.Unlocked);
    }
}
