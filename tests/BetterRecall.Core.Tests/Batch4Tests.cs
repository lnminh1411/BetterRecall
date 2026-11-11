// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Security.Cryptography;
using System.Text;
using System.Text.Json;
using BetterRecall.Contracts;
using BetterRecall.Core.Ai;
using BetterRecall.Core.Crypto;
using BetterRecall.Core.Ipc;

namespace BetterRecall.Core.Tests;

/// <summary>Key vault stand-in with a loaded key and a settable Hello availability.</summary>
internal sealed class FakeKeys : IKeyVault
{
    public bool Hello;
    public string Protection => "dpapi";
    public bool HelloAvailable => Hello;
    public bool KeyLoaded => true;
    public bool UnlockWithPassphrase(string passphrase) => false;
    public Task<bool> VerifyUserAsync(nint hwnd, string reason, CancellationToken ct) => Task.FromResult(true);
    public void DestroyKey() { }
}

public sealed class UnlockPasswordTests : IDisposable
{
    private readonly TempHome _home = new(null);
    private DateTimeOffset _now = DateTimeOffset.UtcNow;

    private UnlockPassword New() => new(_home.Path, () => _now, memKiB: 1024, passes: 1, lanes: 1);

    public void Dispose() => _home.Dispose();

    [Fact]
    public void Set_verify_and_clear()
    {
        var pw = New();
        Assert.False(pw.IsSet);
        Assert.Equal(PasswordCheck.NotSet, pw.Verify("anything"));
        pw.Set("correct horse");
        Assert.True(pw.IsSet);
        Assert.Equal(PasswordCheck.Ok, pw.Verify("correct horse"));
        Assert.Equal(PasswordCheck.Wrong, pw.Verify("Correct horse"));
        Assert.Equal(PasswordCheck.Wrong, pw.Verify(""));
        pw.Clear();
        Assert.False(pw.IsSet);
        Assert.False(File.Exists(DataPaths.UnlockPasswordFile(_home.Path)));
    }

    [Fact]
    public void File_holds_no_password_and_new_salt_each_time()
    {
        var pw = New();
        pw.Set("correct horse");
        var a = File.ReadAllBytes(DataPaths.UnlockPasswordFile(_home.Path));
        Assert.Equal(-1, Encoding.UTF8.GetString(a).IndexOf("correct", StringComparison.Ordinal));
        Assert.Equal(-1, Encoding.Unicode.GetString(a).IndexOf("correct", StringComparison.Ordinal));
        pw.Set("correct horse");
        var b = File.ReadAllBytes(DataPaths.UnlockPasswordFile(_home.Path));
        Assert.False(a.AsSpan().SequenceEqual(b));
    }

    [Fact]
    public void Rate_limit_after_five_wrong_tries_survives_restart_and_doubles()
    {
        var pw = New();
        pw.Set("secret-1");
        for (int i = 0; i < 4; i++) Assert.Equal(PasswordCheck.Wrong, pw.Verify("nope"));
        Assert.Equal(0, pw.RetryAfterSeconds);
        Assert.Equal(PasswordCheck.Wrong, pw.Verify("nope"));        // 5th: 30 s wait starts
        Assert.Equal(30, pw.RetryAfterSeconds);
        var again = New();                                             // agent restart
        Assert.Equal(PasswordCheck.Throttled, again.Verify("secret-1"));
        _now += TimeSpan.FromSeconds(31);
        Assert.Equal(PasswordCheck.Wrong, again.Verify("nope"));      // 6th: 60 s
        Assert.Equal(60, again.RetryAfterSeconds);
        _now += TimeSpan.FromSeconds(61);
        Assert.Equal(PasswordCheck.Ok, again.Verify("secret-1"));
        Assert.Equal(PasswordCheck.Wrong, again.Verify("nope"));      // counter was reset
        Assert.Equal(0, again.RetryAfterSeconds);
    }

    [Fact]
    public void Damaged_file_counts_as_not_set()
    {
        var path = DataPaths.UnlockPasswordFile(_home.Path);
        Directory.CreateDirectory(Path.GetDirectoryName(path)!);
        File.WriteAllBytes(path, [1, 2, 3]);
        Assert.Equal(PasswordCheck.NotSet, New().Verify("x"));
    }

    [Theory]
    [InlineData("12345", false)]
    [InlineData("123456", true)]
    [InlineData("      ", false)]
    public void New_password_rule(string pw, bool ok) => Assert.Equal(ok, UnlockPassword.ValidNew(pw));
}

public sealed class Batch4CommandTests : IAsyncDisposable
{
    private readonly TempStore _t = new();
    private readonly TempHome _home = new(null);
    private readonly SettingsStore _settings;
    private readonly FakeKeys _keys = new();
    private readonly FakeAgent _agent = new();
    private readonly FakeHttpServer _http = new();
    private readonly CommandProcessor _p;
    private readonly ClientSession _ui = new(PipeKind.Ui, 101, @"C:\app\ui\BetterRecall.UI.exe");
    private DateTimeOffset _now = DateTimeOffset.UtcNow;
    private readonly string _shot;
    private static readonly byte[] Image = [.. "RIFF"u8, 0, 0, 0, 0, .. "WEBPVP8 test-image-bytes"u8];

    public Batch4CommandTests()
    {
        _settings = SettingsStore.Load(_home.Path);
        _p = new CommandProcessor(new CommandDeps
        {
            Settings = _settings,
            Store = () => _t.Store,
            Keys = _keys,
            Agent = _agent,
            Embedder = new FakeEmbedder(),
            Thumbnailer = new FakeThumbs(),
            PurgeAll = _ => Task.FromResult(0),
            Password = new UnlockPassword(_home.Path, () => _now, memKiB: 1024, passes: 1, lanes: 1),
            AiKey = new ApiKeyStore(_home.Path),
            Describer = new CloudDescriber(),
        })
        { Now = () => _now };
        _shot = _t.Store.AddScreenCapture(Caps.Screen(DateTimeOffset.UtcNow.AddMinutes(-3), "chrome.exe", "Acme invoice", image: Image));
        _t.Store.SetEmbedding(_shot, Vec.Seeded(3), Vec.Info);
    }

    public async ValueTask DisposeAsync()
    {
        await _http.DisposeAsync();
        _t.Dispose();
        _home.Dispose();
    }

    private async Task<(bool Ok, JsonElement Data, string? Error)> Call(string line)
    {
        var r = await _p.HandleAsync(_ui, line, CancellationToken.None);
        Assert.DoesNotContain('\n', r.Line);
        return Envelope.Parse(r.Line);
    }

    private void Set(string json) => Assert.Null(_settings.TryUpdate(JsonDocument.Parse(json).RootElement));

    // ---------------- unlock password ----------------

    [Fact]
    public async Task Password_method_needs_a_password_first()
    {
        var r = await Call("settings set {\"security\":{\"unlockMethod\":\"password\"}}");
        Assert.Equal(CommandProcessor.ErrNoPassword, r.Error);
        Assert.Equal("hello", _settings.Current.Security.UnlockMethod);
    }

    [Fact]
    public async Task Password_unlock_opens_the_gate_without_hello()
    {
        Assert.True((await Call("password set {\"new\":\"open sesame\"}")).Ok);
        Assert.True((await Call("settings set {\"security\":{\"unlockMethod\":\"password\"}}")).Ok);
        _agent.Unlocked = false;

        Assert.Equal(CommandProcessor.ErrWrongPassword, (await Call("unlock open sesam")).Error);
        Assert.False(_agent.Unlocked);
        var ok = await Call("unlock open sesame");
        Assert.True(ok.Ok, ok.Error);
        Assert.True(ok.Data.GetProperty("unlocked").GetBoolean());
        Assert.Contains("opengate", _agent.Calls);
        Assert.DoesNotContain("unlock", _agent.Calls);   // no Windows Hello prompt

        var audit = _t.Store.ReadAudit(20).Select(a => (a.Verb, a.ResultCount)).ToList();
        Assert.Contains(("unlock password", 1), audit);
        Assert.Contains(("unlock password", 0), audit);
        Assert.DoesNotContain(_t.Store.ReadAudit(50), a => a.Verb.Contains("sesame") || a.QuerySha256.Length > 0 && a.Verb.StartsWith("unlock", StringComparison.Ordinal));
    }

    [Fact]
    public async Task Password_unlock_is_rate_limited()
    {
        Assert.True((await Call("password set {\"new\":\"open sesame\"}")).Ok);
        Set("{\"security\":{\"unlockMethod\":\"password\"}}");
        for (int i = 0; i < 4; i++) Assert.Equal(CommandProcessor.ErrWrongPassword, (await Call("unlock nope")).Error);
        Assert.Equal(CommandProcessor.ErrTooMany(30), (await Call("unlock nope")).Error);
        Assert.Equal(CommandProcessor.ErrTooMany(30), (await Call("unlock open sesame")).Error);   // even the right one waits
        var st = await Call("password");
        Assert.True(st.Data.GetProperty("set").GetBoolean());
        Assert.Equal(30, st.Data.GetProperty("retryAfterS").GetInt32());
        _now += TimeSpan.FromSeconds(31);
        Assert.True((await Call("unlock open sesame")).Ok);
    }

    [Fact]
    public async Task First_password_needs_the_gate_when_hello_exists()
    {
        _keys.Hello = true;
        _agent.Unlocked = false;
        Assert.Equal(CommandProcessor.ErrUnlockRequired, (await Call("password set {\"new\":\"open sesame\"}")).Error);
        _keys.Hello = false;   // no Hello on this PC: the user must be able to set one
        Assert.True((await Call("password set {\"new\":\"open sesame\"}")).Ok);
    }

    [Fact]
    public async Task Change_and_clear_need_the_current_password()
    {
        Assert.True((await Call("password set {\"new\":\"first-pass\"}")).Ok);
        Assert.Equal(CommandProcessor.ErrWrongPassword, (await Call("password set {\"new\":\"second-pass\"}")).Error);
        Assert.Equal(CommandProcessor.ErrWrongPassword, (await Call("password set {\"current\":\"bad\",\"new\":\"second-pass\"}")).Error);
        Assert.Equal(CommandProcessor.ErrPasswordRule, (await Call("password set {\"current\":\"first-pass\",\"new\":\"short\"}")).Error);
        Assert.True((await Call("password set {\"current\":\"first-pass\",\"new\":\"second-pass\"}")).Ok);

        Set("{\"security\":{\"unlockMethod\":\"password\"}}");
        Assert.Equal(CommandProcessor.ErrPasswordInUse, (await Call("password clear {\"current\":\"second-pass\"}")).Error);
        Set("{\"security\":{\"unlockMethod\":\"hello\"}}");
        Assert.True((await Call("password clear {\"current\":\"second-pass\"}")).Ok);
        Assert.False((await Call("password")).Data.GetProperty("set").GetBoolean());
        Assert.Equal(CommandProcessor.UsagePassword, (await Call("password reset")).Error);
    }

    [Fact]
    public async Task Hello_method_unchanged()
    {
        _agent.Unlocked = false;
        Assert.True((await Call("unlock")).Ok);
        Assert.Contains("unlock", _agent.Calls);
        Assert.DoesNotContain("opengate", _agent.Calls);
    }

    // ---------------- settings ----------------

    [Theory]
    [InlineData("{\"ai\":{\"endpoint\":\"http://example.com\"}}", "Invalid ai.endpoint.")]
    [InlineData("{\"ai\":{\"endpoint\":\"https://user:pw@example.com\"}}", "Invalid ai.endpoint.")]
    [InlineData("{\"ai\":{\"endpoint\":\"https://example.com/v1?key=1\"}}", "Invalid ai.endpoint.")]
    [InlineData("{\"ai\":{\"provider\":\"mistral\"}}", "Invalid ai.provider.")]
    [InlineData("{\"ai\":{\"model\":\"bad model\"}}", "Invalid ai.model.")]
    [InlineData("{\"security\":{\"unlockMethod\":\"pin\"}}", "Invalid security.unlockMethod.")]
    [InlineData("{\"replay\":{\"sampleSeconds\":1}}", "Invalid replay.sampleSeconds.")]
    [InlineData("{\"ai\":{\"apiKey\":\"x\"}}", "Invalid ai.apiKey.")]
    public void Bad_settings_rejected(string json, string error) => Assert.Equal(error, _settings.TryUpdate(JsonDocument.Parse(json).RootElement));

    [Theory]
    [InlineData("{\"ai\":{\"endpoint\":\"https://proxy.example.com/v1\"}}")]
    [InlineData("{\"ai\":{\"endpoint\":\"http://127.0.0.1:11434/v1\"}}")]
    [InlineData("{\"ai\":{\"endpoint\":\"http://localhost:1234\"}}")]
    [InlineData("{\"ai\":{\"provider\":\"openai-compatible\",\"model\":\"models/gemini-3.8-flash\"}}")]
    [InlineData("{\"replay\":{\"searchable\":false,\"sampleSeconds\":60}}")]
    public void Good_settings_accepted(string json) => Set(json);

    [Fact]
    public void Defaults_are_off_and_private()
    {
        var s = new Settings();
        Assert.False(s.Ai.Enabled);
        Assert.Equal("hello", s.Security.UnlockMethod);
        Assert.True(s.Replay.Searchable);
        Assert.Equal(5, s.Replay.SampleSeconds);
        Assert.Null(SettingsStore.Validate(s));
    }

    // ---------------- API key ----------------

    [Fact]
    public async Task Api_key_is_encrypted_and_never_returned()
    {
        const string key = "sk-test-NOT-A-REAL-KEY-0123456789";
        var r = await Call("ai setkey " + key);
        Assert.True(r.Ok, r.Error);
        Assert.True(r.Data.GetProperty("keySet").GetBoolean());
        Assert.DoesNotContain(key, r.Data.GetRawText());

        var file = File.ReadAllBytes(DataPaths.AiKeyFile(_home.Path));
        Assert.Equal(-1, Encoding.ASCII.GetString(file).IndexOf("NOT-A-REAL", StringComparison.Ordinal));
        Assert.Equal(key, new ApiKeyStore(_home.Path).Read());

        var settings = await Call("settings get");
        Assert.DoesNotContain(key, settings.Data.GetRawText());
        Assert.DoesNotContain(key, File.ReadAllText(DataPaths.SettingsFile(_home.Path)));
        Assert.DoesNotContain(_t.Store.ReadAudit(50), a => a.Verb.Contains("NOT-A-REAL") || a.QuerySha256.Contains("NOT-A-REAL"));

        Assert.Equal(CommandProcessor.ErrApiKeyRule, (await Call("ai setkey has space")).Error);
        r = await Call("ai clearkey");
        Assert.False(r.Data.GetProperty("keySet").GetBoolean());
        Assert.False(File.Exists(DataPaths.AiKeyFile(_home.Path)));
    }

    // ---------------- describe ----------------

    private void EnableAi(string provider, string? model = null)
    {
        Set($"{{\"ai\":{{\"enabled\":true,\"provider\":\"{provider}\",\"endpoint\":\"{_http.BaseUrl}\"{(model is null ? "" : $",\"model\":\"{model}\"")}}}}}");
    }

    private async Task<string> PreviewToken()
    {
        var p = await Call("describe preview " + _shot);
        Assert.True(p.Ok, p.Error);
        Assert.Equal(Convert.ToBase64String(Image), p.Data.GetProperty("base64").GetString());
        Assert.False(p.Data.GetProperty("redacted").GetBoolean());
        return p.Data.GetProperty("token").GetString()!;
    }

    [Fact]
    public async Task Describe_is_off_by_default_and_needs_preview_first()
    {
        Assert.Equal(CommandProcessor.ErrAiOff, (await Call("describe preview " + _shot)).Error);
        EnableAi("gemini");
        Assert.Equal(CommandProcessor.ErrNoApiKey, (await Call("describe preview " + _shot)).Error);
        await Call("ai setkey test-key-1");
        Assert.Equal(CommandProcessor.ErrToken, (await Call("describe send 00112233445566778899aabbccddeeff")).Error);
        var token = await PreviewToken();
        _now += TimeSpan.FromSeconds(121);
        Assert.Equal(CommandProcessor.ErrToken, (await Call("describe send " + token)).Error);
        Assert.Empty(_http.Requests);   // nothing left the PC
        Assert.Equal(CommandProcessor.ErrNoImage, (await Call("describe preview missing-id")).Error);
    }

    [Fact]
    public async Task Describe_locked_store_refused()
    {
        EnableAi("gemini");
        await Call("ai setkey test-key-1");
        _agent.Unlocked = false;
        Assert.Equal(CommandProcessor.ErrLocked, (await Call("describe preview " + _shot)).Error);
    }

    [Fact]
    public async Task Describe_gemini_request_shape()
    {
        EnableAi("gemini");
        await Call("ai setkey gem-key-123");
        _http.ResponseBody = """{"candidates":[{"content":{"parts":[{"text":"A Gmail inbox "},{"text":"with an invoice."}],"role":"model"}}]}""";
        var token = await PreviewToken();
        var r = await Call("describe send " + token);
        Assert.True(r.Ok, r.Error);
        Assert.Equal("A Gmail inbox with an invoice.", r.Data.GetProperty("text").GetString());
        Assert.Equal(AiDefaults.Model("gemini"), r.Data.GetProperty("model").GetString());

        var req = Assert.Single(_http.Requests);
        Assert.Equal("POST", req.Method);
        Assert.Equal("/v1beta/models/gemini-3.8-flash:generateContent", req.Path);
        Assert.Equal("gem-key-123", req.Headers["x-goog-api-key"]);
        using var body = JsonDocument.Parse(req.Body);
        var parts = body.RootElement.GetProperty("contents")[0].GetProperty("parts");
        Assert.Equal(CloudDescriber.Prompt, parts[0].GetProperty("text").GetString());
        Assert.Equal("image/webp", parts[1].GetProperty("inlineData").GetProperty("mimeType").GetString());
        Assert.Equal(Convert.ToBase64String(Image), parts[1].GetProperty("inlineData").GetProperty("data").GetString());

        // One audit entry per send, with the hash of the image that left.
        var a = _t.Store.ReadAudit(10).First(x => x.Verb == "describe gemini");
        Assert.Equal(1, a.ResultCount);
        Assert.Equal(Convert.ToHexStringLower(SHA256.HashData(Image)), a.QuerySha256);
        // The token is single use.
        Assert.Equal(CommandProcessor.ErrToken, (await Call("describe send " + token)).Error);
    }

    [Fact]
    public async Task Describe_openai_compatible_request_shape_and_no_key_needed()
    {
        EnableAi("openai-compatible", "llava:13b");
        _http.ResponseBody = """{"choices":[{"index":0,"message":{"role":"assistant","content":"Code editor with C#."}}]}""";
        var r = await Call("describe send " + await PreviewToken());
        Assert.True(r.Ok, r.Error);
        Assert.Equal("Code editor with C#.", r.Data.GetProperty("text").GetString());
        var req = Assert.Single(_http.Requests);
        Assert.Equal("/chat/completions", req.Path);
        Assert.False(req.Headers.ContainsKey("Authorization"));   // local server without a key
        using var body = JsonDocument.Parse(req.Body);
        Assert.Equal("llava:13b", body.RootElement.GetProperty("model").GetString());
        var content = body.RootElement.GetProperty("messages")[0].GetProperty("content");
        Assert.Equal("text", content[0].GetProperty("type").GetString());
        Assert.Equal("image_url", content[1].GetProperty("type").GetString());
        Assert.Equal("data:image/webp;base64," + Convert.ToBase64String(Image), content[1].GetProperty("image_url").GetProperty("url").GetString());

        await Call("ai setkey oa-key-9");
        r = await Call("describe send " + await PreviewToken());
        Assert.True(r.Ok, r.Error);
        Assert.Equal("Bearer oa-key-9", _http.Requests[1].Headers["Authorization"]);
    }

    [Fact]
    public async Task Describe_claude_request_shape()
    {
        EnableAi("claude");
        await Call("ai setkey ant-key-7");
        _http.ResponseBody = """{"id":"msg_1","type":"message","role":"assistant","content":[{"type":"text","text":"A spreadsheet."}],"stop_reason":"end_turn"}""";
        var r = await Call("describe send " + await PreviewToken());
        Assert.True(r.Ok, r.Error);
        Assert.Equal("A spreadsheet.", r.Data.GetProperty("text").GetString());
        var req = Assert.Single(_http.Requests);
        Assert.Equal("/v1/messages", req.Path);
        Assert.Equal("ant-key-7", req.Headers["x-api-key"]);
        Assert.Equal(CloudDescriber.ClaudeVersion, req.Headers["anthropic-version"]);
        using var body = JsonDocument.Parse(req.Body);
        Assert.Equal("claude-haiku-5-5", body.RootElement.GetProperty("model").GetString());
        Assert.Equal(1024, body.RootElement.GetProperty("max_tokens").GetInt32());
        var content = body.RootElement.GetProperty("messages")[0].GetProperty("content");
        Assert.Equal("image", content[0].GetProperty("type").GetString());
        Assert.Equal("base64", content[0].GetProperty("source").GetProperty("type").GetString());
        Assert.Equal("image/webp", content[0].GetProperty("source").GetProperty("media_type").GetString());
        Assert.Equal("text", content[1].GetProperty("type").GetString());
    }

    [Fact]
    public async Task Describe_provider_error_is_short_and_hides_the_key()
    {
        EnableAi("claude");
        await Call("ai setkey ant-secret-key-55");
        _http.Status = 401;
        _http.ResponseBody = """{"type":"error","error":{"type":"authentication_error","message":"invalid x-api-key ant-secret-key-55"}}""";
        var r = await Call("describe send " + await PreviewToken());
        Assert.False(r.Ok);
        Assert.Equal("Provider error (HTTP 401): invalid x-api-key ***", r.Error);
        var a = _t.Store.ReadAudit(10).First(x => x.Verb == "describe claude");
        Assert.Equal(0, a.ResultCount);
    }

    [Fact]
    public async Task Describe_unreachable_provider()
    {
        var port = new Uri(_http.BaseUrl).Port;
        await _http.DisposeAsync();
        Set($"{{\"ai\":{{\"enabled\":true,\"provider\":\"openai-compatible\",\"endpoint\":\"http://127.0.0.1:{port}\"}}}}");
        var r = await Call("describe send " + await PreviewToken());
        Assert.Equal(CommandProcessor.ErrProviderUnreachable, r.Error);
    }

    [Fact]
    public async Task Redactor_output_is_what_is_previewed_and_sent()
    {
        var p = new CommandProcessor(new CommandDeps
        {
            Settings = _settings, Store = () => _t.Store, Keys = _keys, Agent = _agent, Embedder = new FakeEmbedder(), Thumbnailer = new FakeThumbs(),
            PurgeAll = _ => Task.FromResult(0), AiKey = new ApiKeyStore(_home.Path), Describer = new CloudDescriber(), Redactor = new BlackOut(),
        });
        EnableAi("openai-compatible");
        _http.ResponseBody = """{"choices":[{"message":{"content":"ok"}}]}""";
        var pv = Envelope.Parse((await p.HandleAsync(_ui, "describe preview " + _shot, CancellationToken.None)).Line);
        Assert.True(pv.Data.GetProperty("redacted").GetBoolean());
        Assert.Equal(Convert.ToBase64String(BlackOut.Bytes), pv.Data.GetProperty("base64").GetString());
        var sent = Envelope.Parse((await p.HandleAsync(_ui, "describe send " + pv.Data.GetProperty("token").GetString(), CancellationToken.None)).Line);
        Assert.True(sent.Ok, sent.Error);
        Assert.Contains(Convert.ToBase64String(BlackOut.Bytes), Assert.Single(_http.Requests).Body);
        Assert.DoesNotContain(Convert.ToBase64String(Image), _http.Requests[0].Body);
    }

    private sealed class BlackOut : IDescribeRedactor
    {
        public static readonly byte[] Bytes = [.. "RIFF"u8, 0, 0, 0, 0, .. "WEBPVP8 redacted"u8];
        public Task<OutgoingImage> RedactAsync(string captureId, OutgoingImage image, CancellationToken ct) =>
            Task.FromResult(new OutgoingImage(Bytes, "image/webp", image.Width, image.Height, true));
    }

    [Fact]
    public void Parse_text_handles_each_shape()
    {
        Assert.Equal("ab", CloudDescriber.ParseText("gemini", """{"candidates":[{"content":{"parts":[{"text":"a"},{"text":"b"}]}}]}"""));
        Assert.Equal("xy", CloudDescriber.ParseText("openai-compatible", """{"choices":[{"message":{"content":[{"type":"text","text":"x"},{"type":"text","text":"y"}]}}]}"""));
        Assert.Equal("t", CloudDescriber.ParseText("claude", """{"content":[{"type":"thinking","thinking":"z"},{"type":"text","text":"t"}]}"""));
        Assert.Null(CloudDescriber.ParseText("gemini", "not json"));
        Assert.Null(CloudDescriber.ParseText("claude", "{}"));
        Assert.Equal("Provider error (HTTP 429): slow down", CloudDescriber.ErrorText(System.Net.HttpStatusCode.TooManyRequests, """[{"error":{"message":"slow down"}}]"""));
        Assert.Equal("Provider error (HTTP 500)", CloudDescriber.ErrorText(System.Net.HttpStatusCode.InternalServerError, "<html>"));
    }

    [Fact]
    public void Help_lists_new_ui_verbs_only_on_ui_pipe()
    {
        var ui = _p.HelpVerbs(PipeKind.Ui);
        Assert.Contains("password", ui);
        Assert.Contains("ai", ui);
        Assert.Contains("describe", ui);
        Assert.DoesNotContain("describe", _p.HelpVerbs(PipeKind.Public));
        Assert.DoesNotContain("password", _p.HelpVerbs(PipeKind.Mcp));
    }

    [Fact]
    public async Task New_verbs_refused_on_public_pipe()
    {
        var pub = new ClientSession(PipeKind.Public, 5, @"C:\x.exe");
        var r = Envelope.Parse((await _p.HandleAsync(pub, "ai setkey abc", CancellationToken.None)).Line);
        Assert.Equal("Unknown command 'ai'.", r.Error);
        r = Envelope.Parse((await _p.HandleAsync(pub, "describe preview " + _shot, CancellationToken.None)).Line);
        Assert.Equal("Unknown command 'describe'.", r.Error);
    }

    [Fact]
    public void Describe_send_is_long_running()
    {
        Assert.True(CommandProcessor.IsLongRunning(PipeKind.Ui, "describe send abc"));
        Assert.False(CommandProcessor.IsLongRunning(PipeKind.Ui, "describe preview abc"));
    }
}

public sealed class ReplayFrameStoreTests
{
    private static readonly DateTimeOffset T0 = DateTimeOffset.UtcNow.AddHours(-1);

    private static NewReplayFrame Frame(string replay, int offsetMs, string title) =>
        new(T0.AddMilliseconds(offsetMs), replay, offsetMs, "game.exe", title, 0, 1920, 1080, Caps.FakeWebp, "image/webp");

    private static string AddReady(TempStore t, NewReplayFrame f, float[] v)
    {
        var id = t.Store.AddReplayFrame(f);
        var job = t.Store.TakePendingEmbeddings(10).Single(j => j.Id == id);
        Assert.Equal(CaptureKind.Replay, job.Kind);
        Assert.NotNull(job.Image);
        t.Store.SetEmbedding(id, v, Vec.Info);
        return id;
    }

    [Fact]
    public void Replay_frames_are_searchable_by_kind_and_carry_offset()
    {
        using var t = new TempStore();
        var v = Vec.Seeded(11);
        var screen = t.Store.AddScreenCapture(Caps.Screen(T0, "game.exe", "boss fight"));
        t.Store.TakePendingEmbeddings(10);
        t.Store.SetEmbedding(screen, v, Vec.Info);
        var frame = AddReady(t, Frame("20261112T100000Z", 15000, "boss fight"), v);

        SearchResponse S(SearchRequest r) => t.Store.Search(r, v, 50, 1 << 20);
        var all = S(new SearchRequest("boss")).Results;
        Assert.Contains(all, h => h.Id == screen);
        var hit = Assert.Single(all, h => h.Id == frame);
        Assert.Equal(CaptureKind.Replay, hit.Kind);
        Assert.Equal("20261112T100000Z", hit.ReplayId);
        Assert.Equal(15000, hit.ReplayOffsetMs);
        Assert.Null(all.Single(h => h.Id == screen).ReplayId);

        Assert.Equal([frame], S(new SearchRequest("boss", Kind: [CaptureKind.Replay])).Results.Select(h => h.Id));
        Assert.Equal([frame], S(new SearchRequest("boss", Kind: [CaptureKind.Replay], Mode: SearchMode.Vector)).Results.Select(h => h.Id));
        Assert.DoesNotContain(S(new SearchRequest("boss", Kind: [CaptureKind.Screen])).Results, h => h.Id == frame);

        var d = t.Store.Get(frame)!;
        Assert.Equal(CaptureKind.Replay, d.Kind);
        Assert.Equal(15000, d.ReplayOffsetMs);
        Assert.NotNull(t.Store.GetKeyframe(frame));
    }

    [Fact]
    public void Timeline_skips_replay_frames_and_delete_removes_them()
    {
        using var t = new TempStore();
        var v = Vec.Seeded(12);
        var screen = t.Store.AddScreenCapture(Caps.Screen(T0, "game.exe", "lobby"));
        t.Store.TakePendingEmbeddings(10);
        t.Store.SetEmbedding(screen, v, Vec.Info);
        var a = AddReady(t, Frame("rA", 0, "lobby"), v);
        AddReady(t, Frame("rA", 5000, "lobby"), v);
        var b = AddReady(t, Frame("rB", 0, "lobby"), v);

        Assert.Equal([screen], t.Store.Timeline(T0.AddHours(-1), T0.AddHours(1), 50).Select(m => m.Id));
        Assert.Equal(2, t.Store.DeleteReplayFrames("rA"));
        Assert.Null(t.Store.Get(a));
        Assert.NotNull(t.Store.Get(b));
        Assert.Equal(0, t.Store.DeleteReplayFrames("rA"));
        Assert.DoesNotContain(t.Store.Search(new SearchRequest("lobby"), v, 50, 1 << 20).Results, h => h.Id == a);
    }

    [Fact]
    public void Migration_is_idempotent_and_keeps_frames()
    {
        using var t = new TempStore();
        var f = AddReady(t, Frame("rC", 2500, "map"), Vec.Seeded(13));
        t.Reopen();
        t.Reopen();
        Assert.Equal(2500, t.Store.Get(f)!.ReplayOffsetMs);
    }
}
