// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Security.Cryptography;
using System.Text.Json;
using BetterRecall.Contracts;
using BetterRecall.UI.Logic;
using CoreIpc = BetterRecall.Core.Ipc;
using CoreSettings = BetterRecall.Core.SettingsStore;
using CoreStore = BetterRecall.Core.Store.EncryptedStore;
using KeyVault = BetterRecall.Core.Crypto.KeyVault;

namespace BetterRecall.Core.Tests;

// Seam tests (sweep 4 B2, review M1): the UI's client logic (MIT, compiled in from src/BetterRecall.UI/Logic) against the
// real agent command processor on a real UI pipe server. They live here, not in UI.Tests, so UI.Tests never links the
// AGPL backend (LICENSING.md); MIT code compiled into this AGPL test project is fine.

/// <summary>Agent control for the seam harness: a read gate and status the test sets.</summary>
internal sealed class SeamAgent : IAgentControl
{
    public volatile bool Unlocked = true;
    public AgentStatus Status = new(true, false, null, null, false, null, new AudioState(false, false), new QueueState(0), "CPU",
        new StoreStats(0, 0, 0, null, null, 0, 0), 30);
    public AgentStatus GetStatus() => Status with { Locked = !Unlocked };
    public void Pause(TimeSpan? duration, string reason) { }
    public void Resume(string reason) { }
    public bool IsUnlocked => Unlocked;
    public Task<bool> UnlockAsync(CancellationToken ct) { Unlocked = true; return Task.FromResult(true); }   // "Hello" passes
    public void Lock() => Unlocked = false;
    public void OpenReadGate() => Unlocked = true;
    public void Consent() { }
    public Task<ReplayClip> SaveReplayAsync(CancellationToken ct) => throw new NotSupportedException();
    public IReadOnlyList<ReplayClip> ListReplays() => [];
    public Task ExportReplayAsync(string id, string path, CancellationToken ct) => throw new NotSupportedException();
    public void DeleteReplay(string id) => throw new KeyNotFoundException();
    public void Panic() { }
    public Task<BenchResult> BenchAsync(CancellationToken ct) => throw new NotSupportedException();
    public event Action<AgentStatus>? StatusChanged;
    public event Action<CaptureMeta>? CaptureStored { add { } remove { } }
    public void RaiseStatus() => StatusChanged?.Invoke(GetStatus());
}

internal sealed class SeamKeys : IKeyVault
{
    public string Protection => "dpapi";
    public bool HelloAvailable => true;
    public bool KeyLoaded => true;
    public bool UnlockWithPassphrase(string passphrase) => false;
    public Task<bool> VerifyUserAsync(nint hwnd, string reason, CancellationToken ct) => Task.FromResult(true);
    public void DestroyKey() { }
}

/// <summary>
/// The agent's real command processor, settings, store and key vault behind a real UI pipe server in this process, so
/// UI logic (AgentClient, AgentApi, StatusSubscription, DescribeSession, OneShotClient) talks to the code it talks to in
/// the product. The mock (MockAgent) got the describe token rule wrong and hid B2; this harness cannot.
/// </summary>
internal sealed class CoreHarness : IAsyncDisposable
{
    public string Home { get; }
    public CoreSettings Settings { get; }
    public CoreStore? Store { get; private set; }
    public SeamAgent Agent { get; } = new();
    public CoreIpc.CommandProcessor Processor { get; }
    public CoreIpc.PipeServer Server { get; }
    private readonly IDisposable? _keysOwned;

    public CoreHarness(string? settingsJson = null, Func<string, CoreSettings, IKeyVault>? keys = null, bool openStore = true)
    {
        Home = Path.Combine(TestEnv.Root, "uiseam-" + Guid.NewGuid().ToString("N")[..12]);
        Directory.CreateDirectory(Home);
        File.WriteAllText(DataPaths.SettingsFile(Home), settingsJson ?? "{\"security\":{\"keyProtection\":\"dpapi\"}}");
        Settings = CoreSettings.Load(Home);
        var vault = keys?.Invoke(Home, Settings) ?? new SeamKeys();
        _keysOwned = vault as IDisposable;
        if (openStore)
        {
            var key = RandomNumberGenerator.GetBytes(32);
            Store = new CoreStore(Home, () => (byte[])key.Clone());
        }
        Processor = new CoreIpc.CommandProcessor(new CoreIpc.CommandDeps
        {
            Settings = Settings, Store = () => Store, Keys = vault, Agent = Agent, Embedder = new FakeEmbedder(), Thumbnailer = new FakeThumbs(),
            PurgeAll = _ => Task.FromResult(0),
            AiKey = new Ai.ApiKeyStore(Home),
            Describer = new Ai.CloudDescriber(),
        });
        if (Store is not null) Store.Purged += Processor.OnStorePurged;   // as CoreHost wires every store
        Server = new CoreIpc.PipeServer("BetterRecall.seamtest.core." + Guid.NewGuid().ToString("N"), CoreIpc.PipeKind.Ui, _ => true, Processor.ServeAsync);
        Server.Start();
    }

    public string PipeName => Server.Name;

    public static ServerTrustVerifier Trust() => new(Environment.ProcessPath!);

    public AgentClient NewClient() => new(PipeName, Trust());

    /// <summary>A stored, indexed screen capture with a keyframe (state 1 needs its embedding).</summary>
    public string AddScreen(string app, string title, int minutesAgo = 2)
    {
        var id = Store!.AddScreenCapture(new NewScreenCapture(
            DateTimeOffset.UtcNow.AddMinutes(-minutesAgo), app, @"C:\Apps\" + app, title, 0, 1280, 720, 0x1234,
            [.. "RIFF"u8, 0, 0, 0, 0, .. "WEBPVP8 MARKER-PIXELS-0123456789"u8], "image/webp", true));
        var v = new float[768];
        v[0] = 1;
        Store.SetEmbedding(id, v, new EmbeddingInfo("fake-gemma", "r1", 768));
        return id;
    }

    public async ValueTask DisposeAsync()
    {
        await Server.DisposeAsync();
        Store?.Dispose();
        _keysOwned?.Dispose();
        Microsoft.Data.Sqlite.SqliteConnection.ClearAllPools();
        for (int i = 0; i < 5; i++)
        {
            try { if (Directory.Exists(Home)) Directory.Delete(Home, true); return; }
            catch (IOException) { await Task.Delay(100); }
            catch (UnauthorizedAccessException) { await Task.Delay(100); }
        }
    }
}

/// <summary>Bug sweep 4, batch U: UI logic against the real agent processor where the seam matters.</summary>
public sealed class Sweep4USeamTests
{
    private const string AiOn = "{\"security\":{\"keyProtection\":\"dpapi\"},\"ai\":{\"enabled\":true,\"provider\":\"openai-compatible\",\"endpoint\":\"http://127.0.0.1:9/v1\",\"model\":\"m\"}}";

    // ---- B2: describe preview and send on one connection

    [Fact]
    public async Task B2_describe_session_reaches_the_provider_on_the_real_agent()
    {
        await using var core = new CoreHarness(AiOn);
        var id = core.AddScreen("notepad.exe", "Quarterly notes");
        await using var session = new DescribeSession(core.NewClient());
        var p = await session.PreviewAsync(id);
        Assert.True(p.Ok, p.Error);
        var r = await session.SendAsync();
        // The token was accepted: the send went on to the provider (a closed loopback port here).
        Assert.False(r.Ok);
        Assert.NotEqual(AgentErrors.InvalidToken, r.Error);
        Assert.Equal(AgentErrors.ProviderUnreachable, r.Error);
    }

    [Fact]
    public async Task B2_old_two_connection_flow_is_refused_by_the_real_agent()
    {
        await using var core = new CoreHarness(AiOn);
        var id = core.AddScreen("notepad.exe", "Quarterly notes");
        await using var media = core.NewClient();
        await using var run = core.NewClient();
        var p = await new AgentApi(media).DescribePreviewAsync(id);
        Assert.True(p.Ok, p.Error);
        var r = await new AgentApi(run).DescribeSendAsync(p.Value!.Token);
        Assert.Equal(AgentErrors.InvalidToken, r.Error);   // tokens live on the connection that asked for the preview
    }

    // ---- M1 (B4): the UI's connections fit under the UI pipe's instance cap; a full pipe is not "agent offline"

    [Fact]
    public async Task M1_ui_connections_fit_the_instance_cap_and_a_full_pipe_falls_back_to_the_main_connection()
    {
        await using var core = new CoreHarness(AiOn);
        Assert.Equal(8, core.Server.MaxInstances);
        var id = core.AddScreen("notepad.exe", "Quarterly notes");
        // What the running UI holds: main, media, the status subscription and an open describe preview.
        await using var main = core.NewClient();
        await using var media = core.NewClient();
        Assert.True((await main.SendAsync("status")).Ok);
        Assert.True((await media.SendAsync("status")).Ok);
        var sub = new StatusSubscription(core.PipeName, CoreHarness.Trust());
        var connected = new TaskCompletionSource(TaskCreationOptions.RunContinuationsAsynchronously);
        sub.Connected += () => connected.TrySetResult();
        sub.Start();
        var held = new List<AgentClient>();
        try
        {
            await connected.Task.WaitAsync(TimeSpan.FromSeconds(10));
            await using var describe = new DescribeSession(core.NewClient());
            Assert.True((await describe.PreviewAsync(id)).Ok);

            // Launcher and history searches, each on its own one-shot connection.
            var kinds = new List<AgentErrorKind>();
            var oneShot = new OneShotClient(core.NewClient, main);
            oneShot.ResponseClassified += k => { lock (kinds) kinds.Add(k); };
            var api = new AgentApi(oneShot);
            var searches = await Task.WhenAll(Enumerable.Range(0, 3).Select(_ => api.SearchAsync(new SearchQuery("quarterly"))));
            Assert.All(searches, s => Assert.True(s.Ok, s.Error));
            Assert.True((await oneShot.SendAsync("history")).Ok);
            lock (kinds) Assert.DoesNotContain(AgentErrorKind.NotRunning, kinds);

            // Fill the pipe: 4 held above plus 4 more.
            for (int i = 0; i < 4; i++)
            {
                var c = core.NewClient();
                held.Add(c);
                Assert.True((await c.SendAsync("status")).Ok);
            }
            var alone = await new OneShotClient(core.NewClient).SendAsync("status");
            Assert.Equal(AgentErrorKind.NotRunning, alone.Kind);   // every instance busy: a new connection waits out its timeout
            var viaMain = await api.SearchAsync(new SearchQuery("quarterly"));
            Assert.True(viaMain.Ok, viaMain.Error);               // read-only: answered on the main connection instead
            lock (kinds) Assert.DoesNotContain(AgentErrorKind.NotRunning, kinds);   // never shown as "agent not running"
        }
        finally
        {
            foreach (var c in held) await c.DisposeAsync();
            await sub.DisposeAsync();
        }
    }

    // ---- B1 + B6 + B8 + B13: the unlock screen follows the agent

    [Fact]
    public async Task B1_unlock_flow_over_the_real_agent_with_a_passphrase_key_under_auto()
    {
        const string pass = "correct horse battery";
        await using var core = new CoreHarness("{\"security\":{\"keyProtection\":\"passphrase\"}}", (home, s) =>
        {
            using (var first = new KeyVault(home, () => s.Current, probeHello: false)) Assert.True(first.UnlockWithPassphrase(pass));
            Assert.Null(s.TryUpdate(JsonDocument.Parse("{\"security\":{\"keyProtection\":\"auto\"}}").RootElement));   // D3
            var v = new KeyVault(home, () => s.Current, probeHello: false);
            Assert.False(v.TryLoadSilent());
            return v;
        }, openStore: false);
        core.Agent.Unlocked = false;
        await using var client = core.NewClient();
        var api = new AgentApi(client);
        var st = (await api.StatusAsync()).Value!;
        Assert.True(st.Locked);
        // Settings say auto and method hello; the old rule showed only the Hello button (B1).
        Assert.Equal(UnlockMode.Hello, UnlockLogic.Mode("auto", "hello", st.PauseReason == "keylocked"));
        Assert.Equal(UnlockMode.Passphrase, UnlockLogic.Mode(st.UnlockNeeds, "auto", "hello", st.PauseReason == "keylocked"));
        // The empty unlock (Hello button) opens no data: status stays locked (B6), the box stays (no flip, no wipe).
        Assert.True((await api.UnlockAsync(null)).Ok);
        st = (await api.StatusAsync()).Value!;
        Assert.True(st.Locked);
        Assert.Equal("passphrase", st.UnlockNeeds);
        Assert.True((await api.UnlockAsync(pass)).Ok);
        st = (await api.StatusAsync()).Value!;
        Assert.False(st.Locked);
        Assert.Equal("none", st.UnlockNeeds);
    }

    // ---- B3: settings reset over the pipe

    [Fact]
    public async Task B3_reset_over_the_real_agent_after_an_unreadable_file()
    {
        await using var core = new CoreHarness("{\"capture\":{\"consented\":true},\"retention\":{\"days\":7}} x");
        await using var client = core.NewClient();
        var api = new AgentApi(client);
        var save = await api.SettingsSetAsync(new System.Text.Json.Nodes.JsonObject { ["ui"] = new System.Text.Json.Nodes.JsonObject { ["theme"] = "dark" } });
        Assert.Equal(AgentErrors.SettingsUnreadable, save.Error);
        Assert.Equal(AgentErrors.SettingsUnreadable, (await api.ConsentAsync()).Error);
        var reset = await api.SettingsResetAsync();
        Assert.True(reset.Ok, reset.Error);
        Assert.True((await api.SettingsSetAsync(new System.Text.Json.Nodes.JsonObject { ["ui"] = new System.Text.Json.Nodes.JsonObject { ["theme"] = "dark" } })).Ok);
    }

    // ---- B10 + A-L5: rows gone reach the UI

    [Fact]
    public async Task B10_status_subscription_hears_removed_rows_from_the_real_agent()
    {
        await using var core = new CoreHarness();
        var keep = core.AddScreen("notepad.exe", "kept", 3);
        var gone = core.AddScreen("bank.exe", "dropped", 1);
        var sub = new StatusSubscription(core.PipeName, CoreHarness.Trust());
        var removed = new TaskCompletionSource<RemovedEvent>(TaskCreationOptions.RunContinuationsAsynchronously);
        var connected = new TaskCompletionSource(TaskCreationOptions.RunContinuationsAsynchronously);
        var statuses = new List<AgentStatus>();
        sub.Connected += () => connected.TrySetResult();
        sub.Removed += e => removed.TrySetResult(e);
        sub.StatusReceived += s => { lock (statuses) statuses.Add(s); };
        sub.Start();
        try
        {
            await connected.Task.WaitAsync(TimeSpan.FromSeconds(10));
            await Task.Delay(100);
            core.Store!.DropCapture(gone, "nsfw");
            var e = await removed.Task.WaitAsync(TimeSpan.FromSeconds(10));
            Assert.Equal("drop", e.Reason);
            Assert.Equal([gone], e.Ids);
            lock (statuses) Assert.All(statuses, s => Assert.NotNull(s.Audio));   // never parsed as a status

            // The UI side: the timeline and the thumbnail cache drop it.
            var match = RemovedMatch.For(e);
            Assert.True(match(gone, DateTimeOffset.UtcNow));
            Assert.False(match(keep, DateTimeOffset.UtcNow));
        }
        finally { await sub.DisposeAsync(); }
    }
}
