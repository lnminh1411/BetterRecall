// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Text.Json;
using BetterRecall.Contracts;
using BetterRecall.Core.Crypto;
using BetterRecall.Core.Ipc;

namespace BetterRecall.Core.Tests;

public sealed class CommandProcessorTests : IDisposable
{
    private readonly TempStore _t = new();
    private readonly TempHome _settingsHome = new(null);
    private readonly SettingsStore _settings;
    private readonly KeyVault _vault;
    private readonly FakeAgent _agent = new();
    private readonly FakeEmbedder _embed = new();
    private readonly FakeThumbs _thumbs = new();
    private readonly CommandProcessor _p;
    private bool _storeOpen = true;
    private int _purgeAllCalls;
    private DateTimeOffset _now = DateTimeOffset.UtcNow;
    private readonly string _id;

    public CommandProcessorTests()
    {
        _settings = SettingsStore.Load(_settingsHome.Path);
        _vault = new KeyVault(_settingsHome.Path, () => _settings.Current, probeHello: false);
        _p = new CommandProcessor(new CommandDeps
        {
            Settings = _settings,
            Store = () => _storeOpen ? _t.Store : null,
            Keys = _vault,
            Agent = _agent,
            Embedder = _embed,
            Thumbnailer = _thumbs,
            PurgeAll = _ => { _purgeAllCalls++; return Task.FromResult(7); },
        })
        { Now = () => _now };
        _id = _t.Store.AddScreenCapture(Caps.Screen(DateTimeOffset.UtcNow.AddMinutes(-5), "chrome.exe", "Acme invoice 4471 - Gmail"));
        _t.Store.SetEmbedding(_id, Vec.Seeded(1), Vec.Info);
        _embed.Map["acme"] = Vec.Seeded(1);
    }

    public void Dispose()
    {
        _vault.Dispose();
        _t.Dispose();
        _settingsHome.Dispose();
    }

    private static readonly ClientSession Pub = new(PipeKind.Public, 100, @"C:\tools\client.exe");
    private ClientSession Ui = new(PipeKind.Ui, 101, @"C:\app\ui\BetterRecall.UI.exe");
    private static readonly ClientSession Mcp = new(PipeKind.Mcp, 102, @"C:\app\betterrecall-mcp.exe");

    private async Task<(bool Ok, JsonElement Data, string? Error)> Call(ClientSession s, string line)
    {
        var r = await _p.HandleAsync(s, line, CancellationToken.None);
        Assert.DoesNotContain('\n', r.Line);
        return Envelope.Parse(r.Line);
    }

    private async Task<string?> Err(ClientSession s, string line) => (await Call(s, line)).Error;

    private void Set(string json) => Assert.Null(_settings.TryUpdate(JsonDocument.Parse(json).RootElement));

    [Fact]
    public async Task Ping_version_and_case_insensitive_verbs()
    {
        var (ok, d, _) = await Call(Pub, "PING");
        Assert.True(ok);
        Assert.True(d.GetProperty("pong").GetBoolean());
        (ok, d, _) = await Call(Pub, "  Version ");
        Assert.Equal(CommandProcessor.AppVersion, d.GetProperty("app").GetString());
        Assert.Matches(@"^\d+\.\d+\.\d+", CommandProcessor.AppVersion);   // from <Version> in Directory.Build.props
        Assert.Equal(1, d.GetProperty("protocol").GetInt32());
    }

    [Fact]
    public async Task Unknown_verbs_per_pipe()
    {
        Assert.Equal("Unknown command 'frobnicate'.", await Err(Pub, "frobnicate now"));
        Assert.Equal("Unknown command 'settings'.", await Err(Pub, "settings get"));
        Assert.Equal("Unknown command 'thumb'.", await Err(Mcp, "thumb x"));
        Assert.Equal("Unknown command 'pause'.", await Err(Mcp, "pause"));
        Assert.Equal("Unknown command 'subscribe'.", await Err(Mcp, "subscribe"));
        Assert.Equal("Unknown command 'help'.", await Err(Mcp, "help"));
        Assert.Equal("Unknown command ''.", await Err(Pub, ""));
    }

    [Fact]
    public async Task Help_lists_only_enabled_verbs()
    {
        var (_, d, _) = await Call(Pub, "help");
        var verbs = d.GetProperty("verbs").EnumerateArray().Select(v => v.GetString()).ToList();
        Assert.DoesNotContain("pause", verbs);
        Assert.DoesNotContain("thumb", verbs);
        Assert.DoesNotContain("purge", verbs);
        Set("{\"ipc\":{\"allowControl\":true,\"allowPurge\":true,\"allowThumbnails\":true}}");
        (_, d, _) = await Call(Pub, "help");
        verbs = d.GetProperty("verbs").EnumerateArray().Select(v => v.GetString()).ToList();
        Assert.Contains("pause", verbs);
        Assert.Contains("thumb", verbs);
        Assert.Contains("purge", verbs);
        (_, d, _) = await Call(Ui, "help");
        Assert.Contains("settings", d.GetProperty("verbs").EnumerateArray().Select(v => v.GetString()));
    }

    [Fact]
    public async Task Status_comes_from_agent()
    {
        var (ok, d, _) = await Call(Mcp, "status");
        Assert.True(ok);
        Assert.True(d.GetProperty("capturing").GetBoolean());
        Assert.Equal("CPU", d.GetProperty("device").GetString());
    }

    [Fact]
    public async Task Search_free_text_and_json_and_result_shape()
    {
        var (ok, d, _) = await Call(Pub, "search acme");
        Assert.True(ok);
        Assert.Equal(1, d.GetProperty("count").GetInt32());
        Assert.False(d.GetProperty("truncated").GetBoolean());
        var r = d.GetProperty("results")[0];
        Assert.Equal(_id, r.GetProperty("id").GetString());
        Assert.Equal("screen", r.GetProperty("kind").GetString());
        Assert.EndsWith("Z", r.GetProperty("ts").GetString());
        Assert.InRange(r.GetProperty("score").GetSingle(), 0f, 1f);
        Assert.False(r.TryGetProperty("durationS", out _));
        Assert.True(r.GetProperty("textMatch").GetBoolean());   // keyword hit is never weak
        Assert.False(r.GetProperty("weak").GetBoolean());
        (ok, d, _) = await Call(Pub, "search {\"q\":\"zzz unrelated\",\"mode\":\"vector\"}");
        Assert.True(ok);
        foreach (var v in d.GetProperty("results").EnumerateArray())
        {
            float cos = v.GetProperty("cosine").GetSingle();
            Assert.Equal(cos < 0.65f, v.GetProperty("weak").GetBoolean());
            Assert.Equal(Math.Max(0f, cos), v.GetProperty("score").GetSingle(), 3);   // vector-only score is the cosine, floored at 0
        }
        (ok, d, _) = await Call(Mcp, "search {\"q\":\"invoice\",\"limit\":500,\"kind\":[\"screen\"],\"mode\":\"text\",\"app\":\"CHROME.EXE\"}");
        Assert.True(ok);
        Assert.Equal(1, d.GetProperty("count").GetInt32());
        // audit: sha256 of the query, never the text
        var audit = _t.Store.ReadAudit(5);
        Assert.Equal("search", audit[0].Verb);
        Assert.Equal(CommandProcessor.Sha256("invoice"), audit[0].QuerySha256);
        Assert.Equal("mcp", audit[0].Pipe);
        Assert.DoesNotContain(audit, a => a.QuerySha256.Contains("invoice") || a.Verb.Contains("invoice"));
    }

    [Theory]
    [InlineData("search", "Usage: search <text> | search <json>")]
    [InlineData("search    ", "Usage: search <text> | search <json>")]
    [InlineData("search {bad json", "Invalid JSON.")]
    [InlineData("search {\"limit\":3}", "Invalid q.")]
    [InlineData("search {\"q\":\"   \"}", "Invalid q.")]
    [InlineData("search {\"q\":5}", "Invalid q.")]
    [InlineData("search {\"q\":\"x\",\"limit\":0}", "Invalid limit.")]
    [InlineData("search {\"q\":\"x\",\"limit\":\"3\"}", "Invalid limit.")]
    [InlineData("search {\"q\":\"x\",\"from\":\"yesterday\"}", "Invalid from.")]
    [InlineData("search {\"q\":\"x\",\"to\":12}", "Invalid to.")]
    [InlineData("search {\"q\":\"x\",\"from\":\"2026-11-10T00:00:00Z\",\"to\":\"2026-11-09T00:00:00Z\"}", "Invalid from.")]
    [InlineData("search {\"q\":\"x\",\"app\":\"\"}", "Invalid app.")]
    [InlineData("search {\"q\":\"x\",\"kind\":[\"video\"]}", "Invalid kind.")]
    [InlineData("search {\"q\":\"x\",\"kind\":\"screen\"}", "Invalid kind.")]
    [InlineData("search {\"q\":\"x\",\"mode\":\"fuzzy\"}", "Invalid mode.")]
    [InlineData("get", "Usage: get <id>")]
    [InlineData("get a b", "Usage: get <id>")]
    [InlineData("get nope", "Not found.")]
    [InlineData("subscribe everything", "Usage: subscribe [captures]")]
    public async Task Public_errors(string line, string error) => Assert.Equal(error, await Err(Pub, line));

    [Fact]
    public async Task Long_query_is_invalid()
        => Assert.Equal("Invalid q.", await Err(Pub, "search " + new string('a', 1025)));

    [Fact]
    public async Task Read_gate_closes_data_verbs()
    {
        _agent.Unlocked = false;
        Assert.Equal("Store is locked.", await Err(Pub, "search acme"));
        Assert.Equal("Store is locked.", await Err(Mcp, "get " + _id));
        Assert.Equal("Store is locked.", await Err(Ui, "timeline 2026-01-01T00:00:00Z 2030-01-01T00:00:00Z"));
        Assert.Equal("Store is locked.", await Err(Ui, "keyframe " + _id));
        Assert.Equal("Store is locked.", await Err(Ui, "audit"));
        Assert.Equal("Store is locked.", await Err(Ui, "replay list"));
        Assert.Equal("Store is locked.", await Err(Pub, "subscribe captures"));
        Assert.True((await Call(Pub, "status")).Ok);   // not a data verb
        Assert.True((await Call(Pub, "subscribe")).Ok); // status stream is not gated
        Set("{\"security\":{\"requireUnlock\":false}}");
        Assert.True((await Call(Pub, "search acme")).Ok);
        // Store not open at all: locked whatever the settings say.
        _storeOpen = false;
        Assert.Equal("Store is locked.", await Err(Pub, "search acme"));
    }

    [Fact]
    public async Task Ipc_requireUnlocked_false_opens_ipc_but_not_ui()
    {
        _agent.Unlocked = false;
        Set("{\"ipc\":{\"requireUnlocked\":false}}");
        Assert.True((await Call(Pub, "search acme")).Ok);
        Assert.Equal("Store is locked.", await Err(Ui, "search acme"));
    }

    [Fact]
    public async Task Rate_limit_per_connection()
    {
        Set("{\"ipc\":{\"maxSearchPerMinute\":3}}");
        var s = new ClientSession(PipeKind.Public, 1, "x");
        for (int i = 0; i < 3; i++) Assert.True((await Call(s, "search acme")).Ok);
        Assert.Equal("Rate limit exceeded.", await Err(s, "search acme"));
        Assert.True((await Call(new ClientSession(PipeKind.Public, 2, "y"), "search acme")).Ok);
        _now = _now.AddSeconds(61);
        Assert.True((await Call(s, "search acme")).Ok);
        for (int i = 0; i < 10; i++) Assert.True((await Call(Ui, "search acme")).Ok); // UI is not rate limited
    }

    [Fact]
    public async Task Embedder_failure_falls_back_to_text_in_hybrid_and_errors_in_vector()
    {
        _embed.Fail = true;
        Assert.True((await Call(Pub, "search acme")).Ok);
        Assert.Equal("Internal error.", await Err(Pub, "search {\"q\":\"acme\",\"mode\":\"vector\"}"));
        _embed.Calls = 0;
        Assert.True((await Call(Pub, "search {\"q\":\"acme\",\"mode\":\"text\"}")).Ok);
        Assert.Equal(0, _embed.Calls); // text mode skips the embedder
    }

    [Fact]
    public async Task Get_returns_text_in_place_of_snippet()
    {
        var (ok, d, _) = await Call(Pub, "get " + _id);
        Assert.True(ok);
        Assert.Equal("Acme invoice 4471 - Gmail", d.GetProperty("text").GetString());
        Assert.False(d.TryGetProperty("snippet", out _));
    }

    [Fact]
    public void Get_text_is_cut_to_max_response_bytes()
    {
        var d = new CaptureDetail("a_1", DateTimeOffset.UtcNow, CaptureKind.Audio, "x.exe", "", new string('é', 100_000), false, 5);
        var json = CommandProcessor.FitDetail(d, 4096);
        Assert.True(System.Text.Encoding.UTF8.GetByteCount(json) <= 4096);
        Assert.Contains("\"text\":\"", json);
    }

    [Fact]
    public async Task Thumb_needs_allowThumbnails_on_public_and_validates_width()
    {
        Assert.Equal("Thumbnails are off (config ipc.allowThumbnails).", await Err(Pub, "thumb " + _id));
        Set("{\"ipc\":{\"allowThumbnails\":true}}");
        var (ok, d, _) = await Call(Pub, "thumb " + _id);
        Assert.True(ok);
        Assert.Equal(320, d.GetProperty("width").GetInt32());
        Assert.Equal("AQID", d.GetProperty("base64").GetString());
        Assert.Equal(640, (await Call(Pub, $"thumb {_id} 640")).Data.GetProperty("width").GetInt32());
        Assert.Equal("Invalid width.", await Err(Pub, $"thumb {_id} 63"));
        Assert.Equal("Invalid width.", await Err(Pub, $"thumb {_id} 1281"));
        Assert.Equal("Usage: thumb <id> [width]", await Err(Pub, $"thumb {_id} wide"));
        Assert.Equal("Usage: thumb <id> [width]", await Err(Pub, "thumb"));
        Assert.Equal("Not found.", await Err(Pub, "thumb missing"));
        Assert.True((await Call(Ui, "thumb " + _id)).Ok); // UI ignores ipc.allow*
    }

    [Fact]
    public async Task Pause_resume_need_allowControl()
    {
        Assert.Equal("Control is off (config ipc.allowControl).", await Err(Pub, "pause"));
        Assert.Equal("Control is off (config ipc.allowControl).", await Err(Pub, "resume"));
        Set("{\"ipc\":{\"allowControl\":true,\"maxPauseS\":600}}");
        Assert.True((await Call(Pub, "pause")).Ok);
        Assert.True((await Call(Pub, "pause 60")).Ok);
        Assert.Equal("Invalid seconds.", await Err(Pub, "pause 601"));
        Assert.Equal("Invalid seconds.", await Err(Pub, "pause 0"));
        Assert.Equal("Usage: pause [seconds]", await Err(Pub, "pause soon"));
        Assert.Equal("Usage: pause [seconds]", await Err(Pub, "pause 1 2"));
        Assert.True((await Call(Pub, "resume")).Ok);
        Assert.Equal(["pause  ipc", "pause 60 ipc", "resume ipc"], _agent.Calls);
        Assert.True((await Call(Ui, "pause 30")).Ok);
        Assert.Equal("pause 30 user", _agent.Calls[^1]);
    }

    [Fact]
    public async Task Purge_two_step_tokens()
    {
        Assert.Equal("Control is off (config ipc.allowControl).", await Err(Pub, "purge last 60"));
        Set("{\"ipc\":{\"allowControl\":true}}");
        Assert.Equal("Purge is off (config ipc.allowPurge).", await Err(Pub, "purge last 60"));
        Set("{\"ipc\":{\"allowPurge\":true}}");
        const string usage = "Usage: purge last <seconds> | purge range <from> <to> | purge app <name> | purge site <host> | purge id <id> | purge confirm <token>";
        Assert.Equal(usage, await Err(Pub, "purge"));
        Assert.Equal(usage, await Err(Pub, "purge last x"));
        Assert.Equal(usage, await Err(Pub, "purge range x y"));
        Assert.Equal(usage, await Err(Pub, "purge all"));
        Assert.Equal(usage, await Err(Pub, "purge app"));
        Assert.Equal("Invalid seconds.", await Err(Pub, "purge last 0"));
        Assert.Equal("Invalid from.", await Err(Pub, "purge range 2026-11-10T00:00:00Z 2026-11-09T00:00:00Z"));

        var s = new ClientSession(PipeKind.Public, 5, "x");
        var (ok, d, _) = await Call(s, "purge last 3600");
        Assert.True(ok);
        Assert.Equal(1, d.GetProperty("rows").GetInt32());
        var token = d.GetProperty("confirm").GetString()!;
        // bound to the connection
        Assert.Equal("Invalid or expired token.", await Err(new ClientSession(PipeKind.Public, 6, "y"), "purge confirm " + token));
        (ok, d, _) = await Call(s, "purge confirm " + token);
        Assert.True(ok);
        Assert.Equal(1, d.GetProperty("purged").GetInt32());
        Assert.Null(_t.Store.Get(_id));
        // single use
        Assert.Equal("Invalid or expired token.", await Err(s, "purge confirm " + token));
        // expiry after 30 s
        token = (await Call(s, "purge app chrome.exe")).Data.GetProperty("confirm").GetString()!;
        _now = _now.AddSeconds(31);
        Assert.Equal("Invalid or expired token.", await Err(s, "purge confirm " + token));
        Assert.Equal("Unknown command 'purge'.", await Err(Mcp, "purge last 60"));
    }

    [Fact]
    public async Task Purge_all_needs_recent_unlock_and_confirm()
    {
        Assert.Equal("Unlock required.", await Err(Ui, "purge all"));
        Assert.True((await Call(Ui, "unlock")).Ok);
        var (ok, d, _) = await Call(Ui, "purge all");
        Assert.True(ok);
        Assert.False(d.TryGetProperty("rows", out _));
        var token = d.GetProperty("confirm").GetString()!;
        (ok, d, _) = await Call(Ui, "purge confirm " + token);
        Assert.True(ok);
        Assert.Equal(7, d.GetProperty("purged").GetInt32());
        Assert.Equal(1, _purgeAllCalls);
        _now = _now.AddSeconds(61);
        Assert.Equal("Unlock required.", await Err(Ui, "purge all"));
    }

    [Fact]
    public async Task Unlock_lock_consent_panic_bench()
    {
        _agent.Unlocked = false;
        var (ok, d, _) = await Call(Ui, "unlock");
        Assert.True(ok);
        Assert.True(d.GetProperty("unlocked").GetBoolean());
        _agent.UnlockResult = false;
        Assert.Equal("Unlock cancelled.", await Err(Ui, "unlock"));
        Assert.True((await Call(Ui, "lock")).Ok);
        Assert.False(_agent.Unlocked);
        Assert.Equal("null", (await Call(Ui, "consent")).Data.GetRawText());
        Assert.True((await Call(Ui, "panic")).Ok);
        Assert.Equal("CPU", (await Call(Ui, "bench")).Data.GetProperty("chosen").GetString());
        Assert.Contains("consent", _agent.Calls);
        Assert.Contains("panic", _agent.Calls);
    }

    [Fact]
    public async Task Unlock_with_passphrase_unwraps_the_key()
    {
        using var h = new TempHome(null);
        var s = SettingsStore.Load(h.Path);
        Assert.Null(s.TryUpdate(JsonDocument.Parse("{\"security\":{\"keyProtection\":\"passphrase\"}}").RootElement));
        using var vault = new KeyVault(h.Path, () => s.Current, probeHello: false);
        Assert.False(vault.TryLoadSilent());
        var p = new CommandProcessor(new CommandDeps
        {
            Settings = s, Store = () => null, Keys = vault, Agent = _agent, Embedder = _embed, Thumbnailer = _thumbs,
            PurgeAll = _ => Task.FromResult(0),
        });
        Assert.Equal("Wrong passphrase.", Envelope.Parse((await p.HandleAsync(Ui, "unlock", default)).Line).Error);
        Assert.True(Envelope.Parse((await p.HandleAsync(Ui, "unlock  my pass phrase ", default)).Line).Ok);
        Assert.True(vault.KeyLoaded);
        vault.Dispose();
        using var vault2 = new KeyVault(h.Path, () => s.Current, probeHello: false);
        var p2 = new CommandProcessor(new CommandDeps
        {
            Settings = s, Store = () => null, Keys = vault2, Agent = _agent, Embedder = _embed, Thumbnailer = _thumbs,
            PurgeAll = _ => Task.FromResult(0),
        });
        Assert.Equal("Wrong passphrase.", Envelope.Parse((await p2.HandleAsync(Ui, "unlock wrong", default)).Line).Error);
        Assert.Equal("Wrong passphrase.", Envelope.Parse((await p2.HandleAsync(Ui, "unlock my pass phrase", default)).Line).Error); // spaces are part of it
        Assert.False(vault2.KeyLoaded);
        Assert.True(Envelope.Parse((await p2.HandleAsync(Ui, "unlock  my pass phrase ", default)).Line).Ok);
    }

    [Fact]
    public async Task Settings_get_and_set()
    {
        var (ok, d, _) = await Call(Ui, "settings get");
        Assert.True(ok);
        Assert.Equal(2000, d.GetProperty("capture").GetProperty("intervalMs").GetInt32());
        (ok, d, _) = await Call(Ui, "settings set {\"retention\":{\"days\":90}}");
        Assert.True(ok);
        Assert.Equal(90, d.GetProperty("retention").GetProperty("days").GetInt32());
        Assert.Equal("Invalid retention.days.", await Err(Ui, "settings set {\"retention\":{\"days\":0}}"));
        Assert.Equal(90, _settings.Current.Retention.Days);
        Assert.Equal("Invalid JSON.", await Err(Ui, "settings set {nope"));
        Assert.Equal("Usage: settings get | settings set <json>", await Err(Ui, "settings"));
        Assert.Equal("Usage: settings get | settings set <json>", await Err(Ui, "settings set"));
    }

    [Fact]
    public async Task Timeline_keyframe_audit_replay()
    {
        var (ok, d, _) = await Call(Ui, "timeline 2020-01-01T00:00:00Z 2099-01-01T00:00:00Z 10");
        Assert.True(ok);
        Assert.Equal(_id, d.GetProperty("items")[0].GetProperty("id").GetString());
        Assert.Equal("Usage: timeline <from> <to> [limit]", await Err(Ui, "timeline yesterday today"));
        Assert.Equal("Invalid from.", await Err(Ui, "timeline 2030-01-01T00:00:00Z 2020-01-01T00:00:00Z"));
        Assert.Equal("Invalid limit.", await Err(Ui, "timeline 2020-01-01T00:00:00Z 2030-01-01T00:00:00Z 0"));

        (ok, d, _) = await Call(Ui, "keyframe " + _id);
        Assert.True(ok);
        Assert.Equal(Convert.ToBase64String(Caps.FakeWebp), d.GetProperty("base64").GetString());
        Assert.Equal("Usage: keyframe <id>", await Err(Ui, "keyframe"));
        Assert.Equal("Not found.", await Err(Ui, "keyframe s_nothing"));

        (ok, d, _) = await Call(Ui, "audit 5");
        Assert.True(ok);
        Assert.Equal("keyframe", d.GetProperty("items")[0].GetProperty("verb").GetString());
        Assert.Equal("Usage: audit [limit]", await Err(Ui, "audit x"));

        Assert.True((await Call(Ui, "replay save")).Ok);
        Assert.Single((await Call(Ui, "replay list")).Data.GetProperty("items").EnumerateArray());
        Assert.True((await Call(Ui, @"replay export r1 C:\Users\me\My Videos\clip.mp4")).Ok);
        Assert.Contains(@"export r1 C:\Users\me\My Videos\clip.mp4", _agent.Calls);
        Assert.Equal("Usage: replay save | replay list | replay export <id> <path> | replay delete <id>", await Err(Ui, "replay export r1 relative.mp4"));
        Assert.Equal("Not found.", await Err(Ui, @"replay export nope C:\x.mp4"));
        Assert.True((await Call(Ui, "replay delete r1")).Ok);
        Assert.Equal("Not found.", await Err(Ui, "replay delete r1"));
        Assert.Equal("Usage: replay save | replay list | replay export <id> <path> | replay delete <id>", await Err(Ui, "replay"));
    }

    [Fact]
    public async Task Exceptions_become_internal_error()
    {
        _storeOpen = true;
        _t.Store.Dispose(); // every store call now throws ObjectDisposedException
        Assert.Equal("Internal error.", await Err(Pub, "get " + _id));
        _t.Reopen();
    }

    [Fact]
    public async Task History_verbs_follow_the_setting_and_are_ui_only()
    {
        Assert.Equal("Unknown command 'history'.", await Err(Pub, "history"));
        Assert.True((await Call(Ui, "unlock")).Ok);
        Assert.True((await Call(Ui, "history add acme invoice")).Ok);
        Assert.True((await Call(Ui, "history add tokyo")).Ok);
        var (ok, d, _) = await Call(Ui, "history");
        Assert.True(ok);
        Assert.Equal(2, d.GetProperty("items").GetArrayLength());
        Assert.True((await Call(Ui, "history delete tokyo")).Ok);
        Assert.Equal("acme invoice", (await Call(Ui, "history")).Data.GetProperty("items")[0].GetProperty("q").GetString());
        Assert.Equal("Usage: history | history add <text> | history delete <text> | history clear", await Err(Ui, "history add"));
        Assert.Equal("Usage: history | history add <text> | history delete <text> | history clear", await Err(Ui, "history clear x"));

        // Off: saved searches are deleted, nothing new is saved, the list is empty.
        Assert.True((await Call(Ui, "settings set {\"privacy\":{\"searchHistory\":false}}")).Ok);
        Assert.Empty(_t.Store.History(10));
        Assert.True((await Call(Ui, "history add standup")).Ok);
        Assert.Equal(0, (await Call(Ui, "history")).Data.GetProperty("items").GetArrayLength());
        Assert.Empty(_t.Store.History(10));
    }

    [Fact]
    public async Task Facets_and_site_search_and_purge_id()
    {
        var site = _t.Store.AddScreenCapture(Caps.Screen(DateTimeOffset.UtcNow.AddMinutes(-4), "chrome.exe", "Acme portal") with { Site = "acme.com" });
        _t.Store.SetEmbedding(site, Vec.Seeded(2), Vec.Info);
        Assert.True((await Call(Ui, "unlock")).Ok);
        var (ok, d, _) = await Call(Ui, "facets");
        Assert.True(ok);
        Assert.Equal("chrome.exe", d.GetProperty("apps")[0].GetProperty("name").GetString());
        Assert.Equal(2, d.GetProperty("apps")[0].GetProperty("count").GetInt32());
        Assert.Equal("acme.com", d.GetProperty("sites")[0].GetProperty("name").GetString());
        Assert.Equal("Usage: facets [<from> <to>]", await Err(Ui, "facets x"));
        Assert.Equal("Unknown command 'facets'.", await Err(Pub, "facets"));

        (ok, d, _) = await Call(Ui, "search {\"q\":\"acme\",\"mode\":\"text\",\"site\":\"www.acme.com\"}");
        Assert.True(ok);
        Assert.Equal(1, d.GetProperty("count").GetInt32());
        Assert.Equal("acme.com", d.GetProperty("results")[0].GetProperty("site").GetString());
        Assert.Equal("Invalid site.", await Err(Ui, "search {\"q\":\"acme\",\"site\":\"not a host\"}"));

        (ok, d, _) = await Call(Ui, "purge id " + site);
        Assert.Equal(1, d.GetProperty("rows").GetInt32());
        Assert.True((await Call(Ui, "purge confirm " + d.GetProperty("confirm").GetString())).Ok);
        Assert.Null(_t.Store.Get(site));
        Assert.NotNull(_t.Store.Get(_id));
    }
}
