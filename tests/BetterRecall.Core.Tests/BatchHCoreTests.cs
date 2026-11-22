// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Text;
using System.Text.Json;
using BetterRecall.Contracts;
using BetterRecall.Core.Ai;
using BetterRecall.Core.Crypto;
using BetterRecall.Core.Ipc;

namespace BetterRecall.Core.Tests;

/// <summary>Bug sweep 2, batch H (core side): request limits, CLI purge, pipe gates, PID reuse, audit flood, history purge,
/// live allow-list changes, cloud body caps.</summary>
public sealed class BatchHCoreTests : IDisposable
{
    private readonly TempStore _t = new();
    private readonly TempHome _settingsHome = new();
    private readonly SettingsStore _settings;
    private readonly KeyVault _vault;
    private readonly FakeAgent _agent = new();
    private readonly CommandProcessor _p;
    private readonly string _id;

    public BatchHCoreTests()
    {
        _settings = SettingsStore.Load(_settingsHome.Path);
        _vault = new KeyVault(_settingsHome.Path, () => _settings.Current, probeHello: false);
        _p = new CommandProcessor(new CommandDeps
        {
            Settings = _settings,
            Store = () => _t.Store,
            Keys = _vault,
            Agent = _agent,
            Embedder = new FakeEmbedder(),
            Thumbnailer = new FakeThumbs(),
            PurgeAll = _ => Task.FromResult(0),
        });
        _id = _t.Store.AddScreenCapture(Caps.Screen(DateTimeOffset.UtcNow.AddMinutes(-5), "chrome.exe", "Acme invoice"));
        _t.Store.SetEmbedding(_id, Vec.Seeded(1), Vec.Info);
    }

    public void Dispose()
    {
        _vault.Dispose();
        _t.Dispose();
        _settingsHome.Dispose();
    }

    private static readonly ClientSession Ui = new(PipeKind.Ui, 101, @"C:\app\ui\BetterRecall.UI.exe");

    private async Task<(bool Ok, JsonElement Data, string? Error)> Call(ClientSession s, string line)
        => Envelope.Parse((await _p.HandleAsync(s, line, CancellationToken.None)).Line);

    private void Set(string json) => Assert.Null(_settings.TryUpdate(JsonDocument.Parse(json).RootElement));

    // ---------------------------------------------------------------- S5: exports are long-running (cancellable)

    [Theory]
    [InlineData("export {\"from\":\"2026-01-01T00:00:00Z\",\"to\":\"2026-01-02T00:00:00Z\",\"path\":\"C:\\\\x.zip\"}", true)]
    [InlineData("replay export r1 C:\\clips\\a b.mp4", true)]
    [InlineData("replay list", false)]
    [InlineData("export", false)]
    public void S5_Export_verbs_run_as_long_requests_on_the_ui_pipe(string line, bool expected)
    {
        Assert.Equal(expected, CommandProcessor.IsLongRunning(PipeKind.Ui, line));
        Assert.False(CommandProcessor.IsLongRunning(PipeKind.Public, line));
    }

    [Fact]
    public async Task S5_Cancelled_export_closes_the_connection_and_leaves_no_file()
    {
        var path = Path.Combine(_t.Home.Path, "out.zip");
        var json = JsonSerializer.Serialize(new { from = "2000-01-01T00:00:00Z", to = "2100-01-01T00:00:00Z", path });
        using var cts = new CancellationTokenSource();
        cts.Cancel();
        var reply = await _p.HandleAsync(Ui, "export " + json, cts.Token);
        Assert.True(reply.Close);
        Assert.False(File.Exists(path));
    }

    // ---------------------------------------------------------------- B10 + D33: CLI purge on one connection

    [Fact]
    public void B10_CliPurge_applies_to_previews_and_takes_yes()
    {
        Assert.True(CliPurge.Applies(["purge", "app", "chrome.exe"]));
        Assert.False(CliPurge.Applies(["purge", "confirm", "abc"]));
        Assert.False(CliPurge.Applies(["purge"]));
        Assert.False(CliPurge.Applies(["search", "x"]));
        var words = new List<string> { "purge", "--yes", "app", "chrome.exe" };
        Assert.True(CliPurge.TakeYes(words));
        Assert.Equal(["purge", "app", "chrome.exe"], words);
    }

    private async Task<(int Code, string Out, string Prompt)> CliPurgeRun(bool yes, bool interactive, string? answer)
    {
        Set("{\"ipc\":{\"allowControl\":true,\"allowPurge\":true}}");
        var session = new ClientSession(PipeKind.Public, 55, @"C:\Tools\betterrecall.exe");   // one connection
        var output = new StringWriter();
        var prompt = new StringWriter();
        int code = await CliPurge.RunAsync(async (l, ct) => (await _p.HandleAsync(session, l, ct)).Line, "purge app chrome.exe",
            yes, interactive, () => answer, output, prompt, default);
        return (code, output.ToString(), prompt.ToString());
    }

    [Fact]
    public async Task B10_CliPurge_with_yes_confirms_on_the_same_connection()
    {
        var (code, output, _) = await CliPurgeRun(yes: true, interactive: false, answer: null);
        Assert.Equal(0, code);
        Assert.Contains("\"purged\":1", output);
        Assert.Null(_t.Store.Get(_id));
    }

    [Fact]
    public async Task B10_CliPurge_prompt_yes_deletes_and_no_keeps()
    {
        var (code, _, prompt) = await CliPurgeRun(yes: false, interactive: true, answer: "no");
        Assert.Equal(1, code);
        Assert.Contains("Delete 1 capture?", prompt);
        Assert.NotNull(_t.Store.Get(_id));
        (code, _, _) = await CliPurgeRun(yes: false, interactive: true, answer: " YES ");
        Assert.Equal(0, code);
        Assert.Null(_t.Store.Get(_id));
    }

    [Fact]
    public async Task B10_CliPurge_without_a_console_or_yes_does_not_delete()
    {
        var (code, _, prompt) = await CliPurgeRun(yes: false, interactive: false, answer: "yes");
        Assert.Equal(1, code);
        Assert.Contains(CliPurge.HintYes, prompt);
        Assert.NotNull(_t.Store.Get(_id));
    }

    // ---------------------------------------------------------------- B11: UI pipe gate

    [Fact]
    public async Task B11_Ui_purge_previews_and_confirms_need_the_read_gate()
    {
        var token = (await Call(Ui, "purge app chrome.exe")).Data.GetProperty("confirm").GetString()!;
        _agent.Unlocked = false;
        Assert.Equal(CommandProcessor.ErrLocked, (await Call(Ui, "purge site example.com")).Error);
        Assert.Equal(CommandProcessor.ErrLocked, (await Call(Ui, "purge confirm " + token)).Error);
        Assert.NotNull(_t.Store.Get(_id));
        // purge all keeps its own recent-unlock rule (panic), not the read gate
        Assert.NotEqual(CommandProcessor.ErrLocked, (await Call(Ui, "purge all")).Error);
    }

    [Theory]
    [InlineData("{\"privacy\":{\"searchHistory\":false}}")]
    [InlineData("{\"ocr\":{\"enabled\":false}}")]
    [InlineData("{\"retention\":{\"days\":1}}")]
    [InlineData("{\"storage\":{\"maxGb\":1}}")]
    [InlineData("{\"mcp\":{\"enabled\":true}}")]
    public async Task B11_Destructive_settings_groups_are_refused_while_locked(string json)
    {
        _agent.Unlocked = false;
        Assert.Equal(CommandProcessor.ErrLocked, (await Call(Ui, "settings set " + json)).Error);
        Assert.True((await Call(Ui, "settings set {\"ui\":{\"theme\":\"dark\"}}")).Ok);
    }

    [Fact]
    public async Task B11_Replay_delete_and_ai_clearkey_need_the_read_gate()
    {
        _agent.Replays.Add(new ReplayClip("r1", DateTimeOffset.UtcNow, 1000, 10, true, @"C:\x.mp4.enc"));
        _agent.Unlocked = false;
        Assert.Equal(CommandProcessor.ErrLocked, (await Call(Ui, "replay delete r1")).Error);
        Assert.Single(_agent.Replays);
        Assert.Equal(CommandProcessor.ErrLocked, (await Call(Ui, "ai clearkey")).Error);
        Assert.Equal(CommandProcessor.ErrLocked, (await Call(Ui, "history delete acme")).Error);
        _agent.Unlocked = true;
        Assert.True((await Call(Ui, "replay delete r1")).Ok);
    }

    // ---------------------------------------------------------------- B14: PID reuse

    [Fact]
    public void B14_Server_check_rejects_a_peer_that_started_after_the_connection()
    {
        var server = new PipeServer("BRtest.unused." + Guid.NewGuid().ToString("N"), PipeKind.Public, _ => true, (_, _) => Task.CompletedTask);
        var self = ProcessInfo.Read(Environment.ProcessId, ProcessInfo.CurrentSession);
        var now = DateTime.UtcNow;
        Assert.NotNull(self.CreatedUtc);
        Assert.True(self.CreatedUtc <= now);
        Assert.Null(server.Check(self, now));
        Assert.Equal("pid reuse", server.Check(self with { CreatedUtc = now.AddSeconds(1) }, now));
        Assert.Equal("pid reuse", server.Check(self with { CreatedUtc = null }, now));
    }

    [Fact]
    public void B14_Client_image_lookup_refuses_a_process_newer_than_the_connection()
    {
        var created = ProcessInfo.Read(Environment.ProcessId, ProcessInfo.CurrentSession).CreatedUtc!.Value;
        Assert.True(ProcessInfo.SamePath(ProcessInfo.ImagePathIfStartedBefore(Environment.ProcessId, DateTime.UtcNow), Environment.ProcessPath));
        Assert.Null(ProcessInfo.ImagePathIfStartedBefore(Environment.ProcessId, created.AddSeconds(-1)));
    }

    // ---------------------------------------------------------------- B15: audit flood

    [Fact]
    public void B15_Throttle_merges_repeats_per_key_and_caps_the_total()
    {
        var th = new AuditThrottle(TimeSpan.FromMinutes(1), maxPerWindow: 3);
        var t0 = DateTimeOffset.UtcNow;
        Assert.Equal(1, th.Admit("a", t0));
        for (int i = 0; i < 50; i++) Assert.Equal(0, th.Admit("a", t0.AddSeconds(1)));
        Assert.Equal(51, th.Admit("a", t0.AddMinutes(1)));   // next row carries the merged repeats
        Assert.Equal(1, th.Admit("b", t0.AddMinutes(1)));
        Assert.Equal(1, th.Admit("c", t0.AddMinutes(1)));
        Assert.Equal(0, th.Admit("d", t0.AddMinutes(1)));     // over the per-window total
        Assert.Equal(2, th.Admit("d", t0.AddMinutes(3)));
    }

    // ---------------------------------------------------------------- B20: history follows app and site purges

    [Theory]
    [InlineData("site", "www.pornhub.com", "pornhub.com", "pornhub")]
    [InlineData("site", "news.bbc.co.uk", "news.bbc.co.uk", "bbc")]
    [InlineData("app", "chrome.exe", "chrome.exe", "chrome")]
    public void B20_History_needles(string kind, string value, string first, string second)
    {
        var scope = kind == "site" ? PurgeScope.ForSite(value) : PurgeScope.ForApp(value);
        Assert.Equal(new[] { first, second }, Store.EncryptedStore.HistoryNeedles(scope));
    }

    [Fact]
    public void B20_App_and_site_purges_forget_matching_saved_searches()
    {
        var now = DateTimeOffset.UtcNow;
        foreach (var q in new[] { "pornhub", "PornHub videos", "chrome tips", "weather", "acme invoice" })
            _t.Store.AddHistory(q, now, 100);
        _t.Store.Purge(PurgeScope.ForSite("www.pornhub.com"));
        Assert.Equal(["acme invoice", "chrome tips", "weather"], _t.Store.History(100).Select(h => h.Q).Order().ToArray());
        _t.Store.Purge(PurgeScope.ForApp("chrome.exe"));
        Assert.Equal(["acme invoice", "weather"], _t.Store.History(100).Select(h => h.Q).Order().ToArray());
    }

    // ---------------------------------------------------------------- B21: permission revocation

    [Fact]
    public void B21_Revoked_means_a_permission_turned_off()
    {
        var on = new IpcSettings { AllowControl = true, AllowPurge = true, AllowThumbnails = true, RequireUnlocked = false };
        Assert.False(PipeManager.Revoked(on, on));
        Assert.True(PipeManager.Revoked(on, new IpcSettings { AllowControl = false, AllowPurge = true, AllowThumbnails = true, RequireUnlocked = false }));
        Assert.True(PipeManager.Revoked(on, new IpcSettings { AllowControl = true, AllowPurge = true, AllowThumbnails = false, RequireUnlocked = false }));
        Assert.True(PipeManager.Revoked(on, new IpcSettings { AllowControl = true, AllowPurge = true, AllowThumbnails = true, RequireUnlocked = true }));
        Assert.False(PipeManager.Revoked(new IpcSettings(), on));   // widening keeps connections
    }

    // ---------------------------------------------------------------- B7: cloud body caps

    private static async Task<List<(string Event, string Data)>> Sse(string text)
    {
        var list = new List<(string, string)>();
        await foreach (var e in SseReader.ReadAsync(new MemoryStream(Encoding.UTF8.GetBytes(text)))) list.Add(e);
        return list;
    }

    [Fact]
    public async Task B7_Sse_reader_keeps_line_endings_and_multi_line_data()
    {
        var e = await Sse(": hi\r\nevent: a\r\ndata: x\r\ndata: y\r\n\r\nevent: b\rdata: z\r\rdata: last\n");
        Assert.Equal(new (string, string)[] { ("a", "x\ny"), ("b", "z"), ("", "last") }, e);
    }

    [Fact]
    public async Task B7_Sse_reader_refuses_an_endless_line_and_an_endless_event()
    {
        var longLine = "data: " + new string('x', SseReader.MaxLineChars + 10);
        var ex = await Assert.ThrowsAsync<ProviderStreamException>(() => Sse(longLine));
        Assert.Equal(SseReader.TooLargeMessage, ex.Message);
        var manyLines = string.Concat(Enumerable.Repeat("data: " + new string('y', 1 << 19) + "\n", 20));   // 10 Mi chars, no blank line
        await Assert.ThrowsAsync<ProviderStreamException>(() => Sse(manyLines));
        var failure = ProviderErrors.FromStream(ex, "m", DateTimeOffset.UtcNow, null);
        Assert.Contains(SseReader.TooLargeMessage, failure.Message);
    }

    [Fact]
    public async Task B7_Error_bodies_are_read_capped()
    {
        using var content = new StringContent(new string('e', 500_000));
        var body = await ProviderHttp.ReadCappedAsync(content, ProviderHttp.MaxErrorBodyBytes, default);
        Assert.Equal(ProviderHttp.MaxErrorBodyBytes, body.Length);
    }

    [Fact]
    public void B7_Huge_retry_hints_do_not_overflow()
    {
        const string body = "{\"error\":{\"status\":\"RESOURCE_EXHAUSTED\",\"message\":\"slow down\",\"details\":[{\"retryDelay\":\"1e300s\"}]}}";
        var (_, _, _, _, delay, _) = ProviderErrors.ParseBody(body);
        Assert.Equal(TimeSpan.FromDays(1), delay);
        var now = DateTimeOffset.UtcNow;
        var f = ProviderErrors.FromHttp(429, body, null, "m", now, null);
        Assert.Equal(now + TimeSpan.FromDays(1), f.RetryAt);
        Assert.Equal(TimeSpan.FromDays(1), ProviderErrors.Clamp(TimeSpan.MaxValue));
        Assert.Equal(TimeSpan.Zero, ProviderErrors.Clamp(TimeSpan.FromSeconds(-5)));
    }
}

/// <summary>B15 and B21 on real pipes (test-named, in-process server; no installed instance).</summary>
public sealed class BatchHPipeTests : IAsyncLifetime
{
    private readonly TempStore _t = new();
    private readonly TempHome _home = new();
    private readonly string _tag = Guid.NewGuid().ToString("N")[..10];
    private SettingsStore _settings = null!;
    private KeyVault _vault = null!;
    private PipeManager _pipes = null!;
    private static readonly string Self = Environment.ProcessPath!;

    public async Task InitializeAsync()
    {
        _settings = SettingsStore.Load(_home.Path);
        Set("{\"ipc\":{\"enabled\":true,\"allowThumbnails\":true}}");
        _vault = new KeyVault(_home.Path, () => _settings.Current, probeHello: false);
        var processor = new CommandProcessor(new CommandDeps
        {
            Settings = _settings, Store = () => _t.Store, Keys = _vault, Agent = new FakeAgent(),
            Embedder = new FakeEmbedder(), Thumbnailer = new FakeThumbs(), PurgeAll = _ => Task.FromResult(0),
        });
        _pipes = new PipeManager(_settings, processor, @"C:\NotTheInstallDir", () => _t.Store, (k, s) => k switch
        {
            PipeKind.Ui => $"BRtest.{_tag}.ui",
            PipeKind.Mcp => $"BRtest.{_tag}.mcp",
            _ => $"BRtest.{_tag}.{s.Ipc.PipeName}",
        });
        await _pipes.StartAsync();
    }

    public async Task DisposeAsync()
    {
        await _pipes.DisposeAsync();
        _vault.Dispose();
        _t.Dispose();
        _home.Dispose();
    }

    private void Set(string json) => Assert.Null(_settings.TryUpdate(JsonDocument.Parse(json).RootElement));

    private async Task<PipeClient> Connect(string name)
    {
        var (r, c) = await PipeClient.ConnectAsync(name, Self, 2000);
        Assert.Equal(PipeConnectResult.Ok, r);
        return c!;
    }

    private string PublicName => $"BRtest.{_tag}.{_settings.Current.Ipc.PipeName}";

    [Fact]
    public async Task B21_Removing_a_client_from_allowClients_drops_its_live_connection()
    {
        await using var c = await Connect(PublicName);
        Assert.True(Envelope.Parse((await c.RequestAsync("ping"))!).Ok);
        Set("{\"ipc\":{\"allowClients\":[" + JsonSerializer.Serialize(Self) + "]}}");   // still allowed: kept
        Assert.True(Envelope.Parse((await c.RequestAsync("ping"))!).Ok);
        Set("{\"ipc\":{\"allowClients\":[\"C:\\\\Tools\\\\other.exe\"]}}");
        string? after;
        try { after = await c.RequestAsync("ping"); } catch (IOException) { after = null; }
        Assert.Null(after);
    }

    [Fact]
    public async Task B21_Turning_a_permission_off_drops_live_connections()
    {
        await using var c = await Connect(PublicName);
        Assert.True(Envelope.Parse((await c.RequestAsync("ping"))!).Ok);
        Set("{\"ipc\":{\"allowThumbnails\":false}}");
        string? after;
        try { after = await c.RequestAsync("ping"); } catch (IOException) { after = null; }
        Assert.Null(after);
        await using var fresh = await Connect(PublicName);    // new connections work under the new settings
        Assert.True(Envelope.Parse((await fresh.RequestAsync("ping"))!).Ok);
    }

    [Fact]
    public async Task B15_Reconnect_and_reject_loops_write_one_audit_row_each()
    {
        for (int i = 0; i < 5; i++)
        {
            await using var c = await Connect(PublicName);
            Assert.True(Envelope.Parse((await c.RequestAsync("ping"))!).Ok);
        }
        for (int i = 0; i < 5; i++)
        {
            await using var c = await Connect($"BRtest.{_tag}.ui");   // the UI pipe rejects this image
            Assert.Equal("Client not allowed.", Envelope.Parse((await c.ReadLineAsync())!).Error);
        }
        await Task.Delay(200);
        var audit = _t.Store.ReadAudit(100);
        Assert.Single(audit, a => a.Verb == "connect" && a.Pipe == "ipc");
        Assert.Single(audit, a => a.Verb == "rejected" && a.Pipe == "ui");
    }
}
