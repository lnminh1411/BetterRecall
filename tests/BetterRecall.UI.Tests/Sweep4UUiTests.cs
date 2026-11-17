// SPDX-License-Identifier: MIT
// Copyright (c) BetterRecall contributors
using System.Security.Cryptography;
using System.Text.Json;
using BetterRecall.Contracts;
using BetterRecall.UI.Logic;
using BetterRecall.UI.Logic.Mock;
using CoreIpc = BetterRecall.Core.Ipc;
using CoreSettings = BetterRecall.Core.SettingsStore;
using CoreStore = BetterRecall.Core.Store.EncryptedStore;
using KeyVault = BetterRecall.Core.Crypto.KeyVault;

namespace BetterRecall.UI.Tests;

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

internal sealed class NoEmbedder : IQueryEmbedder
{
    public Task<float[]> EmbedQueryAsync(string text, CancellationToken ct) => Task.FromException<float[]>(new InvalidOperationException("no worker"));
}

internal sealed class NoThumbs : IThumbnailer
{
    public Task<ThumbData?> GetThumbAsync(string id, int width, CancellationToken ct) => Task.FromResult<ThumbData?>(null);
    public void ClearCache() { }
}

/// <summary>
/// The agent's real command processor, settings, store and key vault behind a real UI pipe server in this process, so
/// UI logic (AgentClient, AgentApi, StatusSubscription, DescribeSession) talks to the code it talks to in the product.
/// The mock (MockAgent) got the describe token rule wrong and hid B2; this harness cannot.
/// </summary>
internal sealed class CoreHarness : IAsyncDisposable
{
    public static readonly string Root = Path.Combine(
        Environment.GetEnvironmentVariable("BR_TEST_TMP") is { Length: > 0 } t ? t : Path.GetTempPath(), "betterrecall-uiseam");

    public string Home { get; }
    public CoreSettings Settings { get; }
    public CoreStore? Store { get; private set; }
    public SeamAgent Agent { get; } = new();
    public CoreIpc.CommandProcessor Processor { get; }
    public CoreIpc.PipeServer Server { get; }
    private readonly IDisposable? _keysOwned;

    public CoreHarness(string? settingsJson = null, Func<string, CoreSettings, IKeyVault>? keys = null, bool openStore = true)
    {
        Home = Path.Combine(Root, "h-" + Guid.NewGuid().ToString("N")[..12]);
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
            Settings = Settings, Store = () => Store, Keys = vault, Agent = Agent, Embedder = new NoEmbedder(), Thumbnailer = new NoThumbs(),
            PurgeAll = _ => Task.FromResult(0),
            AiKey = new BetterRecall.Core.Ai.ApiKeyStore(Home),
            Describer = new BetterRecall.Core.Ai.CloudDescriber(),
        });
        if (Store is not null) Store.Purged += n => Processor.GetType()
            .GetMethod("OnStorePurged", System.Reflection.BindingFlags.Instance | System.Reflection.BindingFlags.NonPublic)!
            .Invoke(Processor, [n]);   // as CoreHost wires every store (internal hook)
        Server = new CoreIpc.PipeServer("BetterRecall.uitest.core." + Guid.NewGuid().ToString("N"), CoreIpc.PipeKind.Ui, _ => true, Processor.ServeAsync);
        Server.Start();
    }

    public string PipeName => Server.Name;

    public AgentClient NewClient() => new(PipeName, MockHarness.OwnProcessTrust());

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

    public void Set(string json) => Assert.Null(Settings.TryUpdate(JsonDocument.Parse(json).RootElement));

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

/// <summary>Bug sweep 4, batch U (control plane): UI side, against the real agent processor where the seam matters.</summary>
public sealed class Sweep4UUiTests
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

    [Fact]
    public async Task B2_mock_keeps_describe_tokens_per_connection_like_the_agent()
    {
        await using var h = new MockHarness();
        var id = (await h.Api.TimelineAsync(DateTimeOffset.UtcNow.AddDays(-30), DateTimeOffset.UtcNow, 50)).Value!
            .First(m => m.HasThumb && m.Kind == CaptureKind.Screen).Id;
        var p = await h.Api.DescribePreviewAsync(id);
        Assert.True(p.Ok, p.Error);
        await using (var other = new AgentClient(h.Server.PipeName, MockHarness.OwnProcessTrust()))
            Assert.Equal(AgentErrors.InvalidToken, (await new AgentApi(other).DescribeSendAsync(p.Value!.Token)).Error);

        await using var session = new DescribeSession(new AgentClient(h.Server.PipeName, MockHarness.OwnProcessTrust()));
        Assert.True((await session.PreviewAsync(id)).Ok);
        var sent = await session.SendAsync();
        Assert.True(sent.Ok, sent.Error);
        Assert.Contains("Mock description", sent.Value!.Text);
    }

    // ---- AB-S1: a slow request does not stall the shared connections

    [Fact]
    public async Task AB_S1_thumbnails_load_while_a_describe_preview_waits()
    {
        using var hold = new ManualResetEventSlim(false);
        await using var server = new FakeLineServer(line =>
        {
            if (line.StartsWith("describe preview", StringComparison.Ordinal)) hold.Wait(TimeSpan.FromSeconds(20));   // agent OCR redaction
            return line.StartsWith("thumb", StringComparison.Ordinal)
                ? Envelope.Ok("{\"id\":\"s_1\",\"mime\":\"image/webp\",\"width\":2,\"height\":2,\"bytes\":\"AAAA\"}")
                : Envelope.Error(AgentErrors.NoImage);
        });
        await using var media = new AgentClient(server.Name, new AcceptAll());
        await using var session = new DescribeSession(new AgentClient(server.Name, new AcceptAll()));
        var preview = session.PreviewAsync("s_1");
        await Task.Delay(200);
        var thumb = await new AgentApi(media).ThumbAsync("s_1").WaitAsync(TimeSpan.FromSeconds(5));
        Assert.True(thumb.Ok, thumb.Error);
        Assert.False(preview.IsCompleted);
        hold.Set();
        await preview.WaitAsync(TimeSpan.FromSeconds(5));
    }

    [Fact]
    public async Task AB_S1_one_shot_search_does_not_hold_the_main_connection()
    {
        using var hold = new ManualResetEventSlim(false);
        await using var server = new FakeLineServer(line =>
        {
            if (line.StartsWith("search", StringComparison.Ordinal)) { hold.Wait(TimeSpan.FromSeconds(20)); return Envelope.Ok("{\"count\":0,\"truncated\":false,\"tookMs\":1,\"results\":[]}"); }
            return Envelope.Ok("{\"pong\":true}");
        });
        await using var main = new AgentClient(server.Name, new AcceptAll());
        var oneShot = new OneShotClient(() => new AgentClient(server.Name, new AcceptAll()));
        AgentErrorKind? classified = null;
        oneShot.ResponseClassified += k => classified = k;
        var search = new AgentApi(oneShot).SearchAsync(new SearchQuery("acme"));
        await Task.Delay(200);
        Assert.True((await main.SendAsync("ping").WaitAsync(TimeSpan.FromSeconds(5))).Ok);   // settings, timeline, unlock go on
        Assert.False(search.IsCompleted);
        hold.Set();
        Assert.True((await search.WaitAsync(TimeSpan.FromSeconds(5))).Ok);
        Assert.Equal(AgentErrorKind.None, classified);
    }

    private sealed class AcceptAll : IServerTrust
    {
        public TrustCheck Verify(Microsoft.Win32.SafeHandles.SafePipeHandle pipe) => new(true, Environment.ProcessId);
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

    [Theory]
    [InlineData("passphrase", "auto", "hello", UnlockMode.Passphrase)]
    [InlineData("password", "auto", "hello", UnlockMode.Password)]      // B13: no Hello, the password stands in
    [InlineData("hello", "auto", "password", UnlockMode.Hello)]         // B8/B15: no password file, Hello stands in
    [InlineData("hello", "passphrase", "hello", UnlockMode.Hello)]
    [InlineData(null, "passphrase", "hello", UnlockMode.Passphrase)]    // an agent without the field: settings decide
    [InlineData("none", "auto", "password", UnlockMode.Password)]
    public void B1_unlock_mode_comes_from_the_status_field(string? needs, string keyProtection, string method, UnlockMode expected) =>
        Assert.Equal(expected, UnlockLogic.Mode(needs, keyProtection, method, keyNeedsPassphrase: false));

    [Fact]
    public void B13_hello_missing_answer_asks_for_the_password()
    {
        Assert.True(UnlockLogic.AsksForPassword(AgentErrors.HelloMissing));
        Assert.False(UnlockLogic.AsksForPassword(AgentErrors.UnlockCancelled));
        Assert.Equal("Err_HelloMissing", SearchViewModel.ServerError(AgentErrors.HelloMissing).Key);   // translated, not raw
        Assert.Equal("Err_SettingsUnreadable", SearchViewModel.ServerError(AgentErrors.SettingsUnreadable).Key);
    }

    [Theory]
    [InlineData(1, false, true)]    // UI started before the agent: InitAsync got nothing (B8)
    [InlineData(1, true, false)]    // InitAsync already loaded them
    [InlineData(2, true, true)]     // reconnect: the agent may have restarted (D2)
    public void B8_settings_reload_on_connect(int connects, bool loaded, bool expected) =>
        Assert.Equal(expected, SettingsReload.OnConnect(connects, loaded));

    [Fact]
    public async Task B8_mock_status_carries_unlock_needs()
    {
        await using var h = new MockHarness(new MockOptions(StartLocked: true, PasswordUnlock: true));
        var st = (await h.Api.StatusAsync()).Value!;
        Assert.True(st.Locked);
        Assert.Equal("password", st.UnlockNeeds);
        await using var pass = new MockHarness(new MockOptions(StartLocked: true, Passphrase: true));
        st = (await pass.Api.StatusAsync()).Value!;
        Assert.True(st.Locked);
        Assert.Equal("passphrase", st.UnlockNeeds);
        Assert.True((await pass.Api.UnlockAsync("any passphrase")).Ok);
        Assert.Equal("none", (await pass.Api.StatusAsync()).Value!.UnlockNeeds);
    }

    // ---- B3: warnings reach the UI

    [Fact]
    public void B3_warnings_map_to_one_banner_each()
    {
        var b = WarningBanners.For(["settings-unreadable", "ner-unavailable", "ocr-failing", "vc-runtime-missing", "ner-unavailable", "new-code"]);
        Assert.Equal(["settings-unreadable", "ner-unavailable", "ocr-failing", "vc-runtime-missing", "new-code"], b.Select(x => x.Code));
        Assert.True(b[0].CanReset && b[0].Error);
        Assert.True(b[1].Error);   // names stored unredacted
        Assert.Equal("Warn_OtherTitle", b[4].TitleKey);
        Assert.Empty(WarningBanners.For(null));
        Assert.True(WarningBanners.Same(null, []));
        Assert.False(WarningBanners.Same(["ocr-failing"], null));
    }

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
        var sub = new StatusSubscription(core.PipeName, MockHarness.OwnProcessTrust());
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

    [Fact]
    public async Task B10_timeline_and_thumbnail_cache_drop_removed_rows()
    {
        var now = DateTimeOffset.UtcNow;
        var rows = new List<CaptureMeta>
        {
            new("s_b", now.AddMinutes(-1), CaptureKind.Screen, "bank.exe", "b", 0, 0, true, true),
            new("s_a", now.AddMinutes(-2), CaptureKind.Screen, "notepad.exe", "a", 0, 0, true, true),
            new("s_w", now.AddMinutes(-30), CaptureKind.Screen, "chrome.exe", "w", 0, 0, true, true),
        };
        var json = JsonSerializer.Serialize(new ItemsReply<CaptureMeta>(rows), ContractsJson.Default.ItemsReplyCaptureMeta);
        var vm = new TimelineViewModel(new AgentApi(new FixedLine(json)));
        vm.Day = DateOnly.FromDateTime(DateTime.Now);
        await vm.LoadAsync();
        Assert.Equal(3, vm.Items.Count);
        Assert.Equal(1, vm.RemoveWhere(RemovedMatch.For(new RemovedEvent("removed", "purge", ["s_b"]))));
        Assert.DoesNotContain(vm.Items, i => i.Id == "s_b");
        Assert.Equal(1, vm.RemoveWhere(RemovedMatch.For(new RemovedEvent("removed", "purge", [], now.AddMinutes(-40), now.AddMinutes(-20)))));
        Assert.Equal(["s_a"], vm.Items.Select(i => i.Id));
        Assert.Equal(1, vm.HourCounts.Sum());

        var cache = new ThumbnailCache(1 << 20);
        cache.Put("s_a", 320, [1, 2]);
        cache.Put("s_a", 1280, [1, 2, 3]);
        cache.Put("s_b", 320, [4]);
        cache.Remove(["s_a"]);
        Assert.Null(cache.TryGet("s_a", 320));
        Assert.Null(cache.TryGet("s_a", 1280));
        Assert.NotNull(cache.TryGet("s_b", 320));
        Assert.Equal(1, cache.Bytes);
    }

    private sealed class FixedLine(string data) : IAgentClient
    {
        public int ServerPid => 1;
        public Task<AgentResult> SendAsync(string line, CancellationToken ct = default) =>
            Task.FromResult(AgentClient.Parse(Envelope.Ok(data)));
    }
}
