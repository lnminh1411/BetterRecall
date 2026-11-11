// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Net;
using System.Net.Http.Headers;
using System.Text;
using System.Text.Json;
using System.Text.Json.Nodes;
using BetterRecall.Contracts;
using BetterRecall.Core.Ai;
using BetterRecall.Core.Ipc;
using BetterRecall.Core.Store;

namespace BetterRecall.Core.Tests;

/// <summary>HTTP handler that answers from a queue (default: 200, empty) and records every request body.</summary>
internal sealed class ScriptHandler : HttpMessageHandler
{
    public readonly Queue<Func<HttpResponseMessage>> Responses = new();
    public readonly List<string> Bodies = [];
    public readonly List<HttpRequestMessage> Requests = [];
    public bool Offline;

    protected override async Task<HttpResponseMessage> SendAsync(HttpRequestMessage request, CancellationToken ct)
    {
        Requests.Add(request);
        Bodies.Add(request.Content == null ? "" : await request.Content.ReadAsStringAsync(ct));
        if (Offline) throw new HttpRequestException(HttpRequestError.ConnectionError, "refused");
        return Responses.Count > 0 ? Responses.Dequeue()() : new HttpResponseMessage(HttpStatusCode.OK) { Content = new StringContent("") };
    }

    public static Func<HttpResponseMessage> Status(int status, string body = "", Action<HttpResponseHeaders>? headers = null) => () =>
    {
        var r = new HttpResponseMessage((HttpStatusCode)status) { Content = new StringContent(body, Encoding.UTF8, "application/json") };
        headers?.Invoke(r.Headers);
        return r;
    };

    public static Func<HttpResponseMessage> Sse(string body) => () =>
        new HttpResponseMessage(HttpStatusCode.OK) { Content = new StringContent(body, Encoding.UTF8, "text/event-stream") };
}

/// <summary>Wire format stand-in: the request body is the transcript (plus tool images as base64), so a test sees exactly
/// what would leave the PC; replies come from a script.</summary>
internal sealed class ScriptProvider : IChatProvider
{
    public readonly Queue<ProviderTurn> Turns = new();
    public readonly List<ToolOutput> Outputs = [];
    public string Name => "script";

    public HttpRequestMessage BuildRequest(ChatRequest r)
    {
        var body = new JsonObject { ["system"] = r.System, ["transcript"] = r.Transcript.DeepClone() };
        var m = new HttpRequestMessage(HttpMethod.Post, "https://provider.test/chat") { Content = new StringContent(body.ToJsonString(), Encoding.UTF8, "application/json") };
        m.Headers.TryAddWithoutValidation("x-api-key", r.ApiKey);
        return m;
    }

    public async Task<ProviderTurn> ReadStreamAsync(Stream body, Action<string> onText, CancellationToken ct)
    {
        await body.CopyToAsync(Stream.Null, ct);
        var t = Turns.Dequeue();
        foreach (var w in t.Text.Split(' ').Select((w, i) => i == 0 ? w : " " + w)) if (w.Length > 0) onText(w);
        return t;
    }

    public void AppendUser(JsonArray transcript, string text) =>
        transcript.Add((JsonNode)new JsonObject { ["role"] = "user", ["text"] = text });

    public void AppendAssistant(JsonArray transcript, ProviderTurn turn) => transcript.Add(turn.Assistant.DeepClone());

    public void AppendToolResults(JsonArray transcript, IReadOnlyList<ToolOutput> results)
    {
        Outputs.AddRange(results);
        var arr = new JsonArray();
        foreach (var r in results)
        {
            var images = new JsonArray();
            foreach (var i in r.Images) images.Add((JsonNode)Convert.ToBase64String(i.Bytes));
            arr.Add((JsonNode)new JsonObject { ["id"] = r.CallId, ["text"] = r.Text, ["images"] = images });
        }
        transcript.Add((JsonNode)new JsonObject { ["role"] = "tool", ["results"] = arr });
    }

    public static ProviderTurn Search(string q = "invoice", int calls = 1) => new(
        new JsonObject { ["role"] = "assistant", ["search"] = q },
        "", Enumerable.Range(1, calls).Select(i => new ToolCall("call" + i, ChatService.ToolName, new JsonObject { ["query"] = q })).ToList(), 100, 10, "tool");

    public static ProviderTurn Answer(string text = "It was the Acme invoice [S1].", string stop = "end") =>
        new(new JsonObject { ["role"] = "assistant", ["text"] = text }, text, [], 200, 20, stop);
}

internal sealed class FixedRedactor(bool redacts) : IDescribeRedactor
{
    public static readonly byte[] Bytes = [.. "RIFF"u8, 0, 0, 0, 0, .. "WEBPVP8 blacked-out"u8];
    public bool RedactsImages => true;
    public Task<OutgoingImage> RedactAsync(string captureId, OutgoingImage image, CancellationToken ct) =>
        Task.FromResult(redacts ? new OutgoingImage(Bytes, "image/webp", image.Width, image.Height, true) : image);
}

public sealed class ChatRedactionTests
{
    private readonly BasicTextRedactor _r = new();

    [Theory]
    [InlineData("mail bob.smith+x@example.co.uk now", "mail [redacted] now")]
    [InlineData("see https://bank.example.com/acct/123?session=abc", "see https://bank.example.com/[redacted]")]
    [InlineData("open https://github.com", "open https://github.com")]
    [InlineData("password: hunter2 ok", "password: [redacted] ok")]
    [InlineData("API key=sk-live-abc", "API key=[redacted]")]
    [InlineData("card 4111 1111 1111 1111 exp", "card [redacted] exp")]
    [InlineData("call +44 20 7946 0958", "call [redacted]")]
    [InlineData("IBAN GB82 WEST 1234 5698 7654 32", "IBAN [redacted]")]
    [InlineData("token abcdefghijklmnopqrstuvwx1234", "token [redacted]")]
    [InlineData("meeting 2026-11-12 at 10:30 room 4012", "meeting 2026-11-12 at 10:30 room 4012")]
    [InlineData("", "")]
    public void Redacts_sensitive_patterns(string input, string expected) => Assert.Equal(expected, _r.Redact(input));
}

public sealed class ChatShareGateTests
{
    private static readonly DateTimeOffset T = new(2026, 11, 12, 10, 0, 0, TimeSpan.Zero);
    private static readonly SearchHit Screen = new("s1", T, CaptureKind.Screen, "chrome.exe", "Inbox - bob@example.com", 1, "snippet", true, null, Site: "mail.example.com");
    private static readonly SearchHit Audio = new("a1", T, CaptureKind.Audio, "teams.exe", "", 1, "my pin: 1234 is secret", false, 30);
    private static readonly SearchHit[] Hits = [Screen, Audio];
    private static CaptureDetail? Detail(string id) => null;
    private static Task<ThumbData?> Thumb(string id, int w, CancellationToken ct) => Task.FromResult<ThumbData?>(new ThumbData(id, "image/webp", w, w / 2, [1, 2, 3]));

    private static Task<SharePlan> Build(string share, IDescribeRedactor? red, int maxHits = 8, int maxImages = 3) =>
        ChatShareGate.BuildAsync(Hits, share, maxHits, maxImages, Detail, Thumb, red, new BasicTextRedactor(), CancellationToken.None);

    [Fact]
    public async Task Metadata_sends_no_title_or_text()
    {
        var p = await Build(AiShare.Metadata, new FixedRedactor(true));
        Assert.Equal(AiShare.Metadata, p.Share);
        Assert.All(p.Hits, h => { Assert.DoesNotContain("title:", h.Text); Assert.DoesNotContain("text:", h.Text); Assert.Null(h.Image); });
        Assert.StartsWith("[S1] ", p.Hits[0].Text);
        Assert.Contains("| screen | app: chrome.exe | site: mail.example.com", p.Hits[0].Text);
        Assert.Contains("| speech transcript | app: teams.exe", p.Hits[1].Text);
        Assert.False(p.ImagesUnavailable);
        Assert.Equal("", ChatShareGate.Item(p.Hits[0], 0, new BasicTextRedactor()).Title);
    }

    [Fact]
    public async Task Text_is_redacted_and_has_no_images()
    {
        var p = await Build(AiShare.Text, new FixedRedactor(true));
        Assert.Contains("title: Inbox - [redacted]", p.Hits[0].Text);
        Assert.Contains("text: my pin: [redacted] is secret", p.Hits[1].Text);
        Assert.All(p.Hits, h => Assert.Null(h.Image));
        Assert.False(p.ImagesUnavailable);
    }

    [Fact]
    public async Task Unknown_share_level_fails_closed_to_metadata()
    {
        var p = await Build("everything", new FixedRedactor(true));
        Assert.Equal(AiShare.Metadata, p.Share);
        Assert.All(p.Hits, h => Assert.DoesNotContain("title:", h.Text));
    }

    [Fact]
    public async Task Images_need_a_redactor_that_redacts()
    {
        var none = await Build(AiShare.TextImages, null);
        Assert.True(none.ImagesUnavailable);
        Assert.All(none.Hits, h => Assert.Null(h.Image));
        Assert.Contains(ChatShareGate.ImagesUnavailableNote, ChatShareGate.ToolText(none));

        var passThrough = await Build(AiShare.TextImages, new PassThroughRedactor());
        Assert.True(passThrough.ImagesUnavailable);
        Assert.All(passThrough.Hits, h => Assert.Null(h.Image));

        var liar = await Build(AiShare.TextImages, new FixedRedactor(false));   // claims to redact, returned the original
        Assert.False(liar.ImagesUnavailable);
        Assert.All(liar.Hits, h => Assert.Null(h.Image));

        var ok = await Build(AiShare.TextImages, new FixedRedactor(true));
        Assert.Equal(FixedRedactor.Bytes, ok.Hits[0].Image!.Bytes);
        Assert.Null(ok.Hits[1].Image);   // audio has no image
        Assert.Equal(FixedRedactor.Bytes, ChatShareGate.Item(ok.Hits[0], 2, new BasicTextRedactor()).Image);
    }

    [Fact]
    public async Task Caps_hits_and_images()
    {
        var p = await Build(AiShare.TextImages, new FixedRedactor(true), maxHits: 1, maxImages: 0);
        Assert.Single(p.Hits);
        Assert.Null(p.Hits[0].Image);
        Assert.False(p.ImagesUnavailable);
    }

    [Fact]
    public void Tool_text_for_empty_and_declined()
    {
        var empty = new SharePlan(AiShare.Text, [], false);
        Assert.Contains("No matching captures", ChatShareGate.ToolText(empty));
        Assert.Contains("did not allow", ChatShareGate.ToolText(empty, declined: true));
    }

    private sealed class PassThroughRedactor : IDescribeRedactor
    {
        public Task<OutgoingImage> RedactAsync(string captureId, OutgoingImage image, CancellationToken ct) => Task.FromResult(image);
    }
}

public sealed class ProviderErrorTests
{
    private const string Key = "test-key-0123456789abcdef";
    private static readonly DateTimeOffset Now = new(2026, 11, 12, 10, 0, 0, TimeSpan.Zero);

    private static ChatFailure Map(int status, string body, Action<HttpResponseHeaders>? h = null)
    {
        using var r = new HttpResponseMessage((HttpStatusCode)status);
        h?.Invoke(r.Headers);
        return ProviderErrors.FromHttp(status, body, r.Headers, "m1", Now, Key);
    }

    [Theory]
    [InlineData(401, """{"error":{"type":"authentication_error","message":"invalid x-api-key"}}""", "auth")]
    [InlineData(400, """{"error":{"code":400,"status":"INVALID_ARGUMENT","message":"API key not valid.","details":[{"@type":"type.googleapis.com/google.rpc.ErrorInfo","reason":"API_KEY_INVALID"}]}}""", "auth")]
    [InlineData(402, "", "quota")]
    [InlineData(429, """{"error":{"type":"insufficient_quota","code":"insufficient_quota","message":"You exceeded your current quota"}}""", "quota")]
    [InlineData(400, """{"error":{"type":"invalid_request_error","message":"Your credit balance is too low","details":{"error_code":"credit_balance_exhausted"}}}""", "bad_request")]
    [InlineData(429, """[{"error":{"code":429,"status":"RESOURCE_EXHAUSTED","message":"Quota exceeded for metric generate_content_free_tier_requests"}}]""", "quota")]
    [InlineData(429, """{"error":{"type":"rate_limit_error","message":"Number of request tokens has exceeded your per-minute rate limit"}}""", "rate")]
    [InlineData(403, """{"error":{"type":"permission_error","message":"no access"}}""", "forbidden")]
    [InlineData(404, """{"error":{"type":"not_found_error","message":"model: m1"}}""", "model")]
    [InlineData(400, """{"error":{"code":"context_length_exceeded","message":"This model's maximum context length is 128000 tokens"}}""", "context")]
    [InlineData(400, """{"error":{"type":"invalid_request_error","message":"prompt is too long: 210000 tokens > 200000 maximum"}}""", "context")]
    [InlineData(413, "", "context")]
    [InlineData(529, """{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}""", "overloaded")]
    [InlineData(503, "", "overloaded")]
    [InlineData(500, "<html>", "server")]
    [InlineData(400, "{}", "bad_request")]
    public void Maps_status_and_body(int status, string body, string code) => Assert.Equal(code, Map(status, body).Code);

    [Fact]
    public void Billing_error_code_is_quota()
    {
        var f = Map(400, """{"error":{"type":"billing_error","message":"Your credit balance is too low"}}""");
        Assert.Equal("quota", f.Code);
        Assert.False(ProviderErrors.Retryable(f));
    }

    [Fact]
    public void Retry_after_header_and_gemini_retry_info()
    {
        var a = Map(429, "", h => h.RetryAfter = new RetryConditionHeaderValue(TimeSpan.FromSeconds(30)));
        Assert.Equal("rate", a.Code);
        Assert.Equal(Now.AddSeconds(30), a.RetryAt);
        Assert.Contains("Try again in 30 s.", a.Message);

        var g = Map(429, """{"error":{"code":429,"status":"RESOURCE_EXHAUSTED","message":"Resource has been exhausted","details":[{"@type":"type.googleapis.com/google.rpc.RetryInfo","retryDelay":"7s"}]}}""");
        Assert.Equal("rate", g.Code);
        Assert.Equal(Now.AddSeconds(7), g.RetryAt);
    }

    [Fact]
    public void Key_never_appears_in_messages()
    {
        var f = Map(401, $$$"""{"error":{"message":"Incorrect API key provided: {{{Key}}}."}}""");
        Assert.Equal("auth", f.Code);
        Assert.DoesNotContain(Key, f.Message);
        Assert.Contains("***", f.Message);
    }

    [Fact]
    public void Stream_errors_map_like_http()
    {
        Assert.Equal("overloaded", ProviderErrors.FromStream(new ProviderStreamException("overloaded_error", "Overloaded"), "m", Now, Key).Code);
        Assert.Equal("rate", ProviderErrors.FromStream(new ProviderStreamException("rate_limit_error", "slow"), "m", Now, Key).Code);
        Assert.Equal("server", ProviderErrors.FromStream(new ProviderStreamException("api_error", "oops"), "m", Now, Key).Code);
    }

    [Fact]
    public void Retry_policy()
    {
        var rate = new ChatFailure("rate", "x");
        Assert.Equal(TimeSpan.FromSeconds(1), ProviderErrors.RetryDelay(rate, 0, Now));
        Assert.Equal(TimeSpan.FromSeconds(2), ProviderErrors.RetryDelay(rate, 1, Now));
        Assert.Null(ProviderErrors.RetryDelay(rate, 2, Now));
        Assert.Equal(TimeSpan.FromSeconds(5), ProviderErrors.RetryDelay(rate with { RetryAt = Now.AddSeconds(5) }, 0, Now));
        Assert.Null(ProviderErrors.RetryDelay(rate with { RetryAt = Now.AddMinutes(5) }, 0, Now));   // too long: tell the user when
        Assert.Equal(TimeSpan.Zero, ProviderErrors.RetryDelay(rate with { RetryAt = Now.AddSeconds(-5) }, 0, Now));
        Assert.NotNull(ProviderErrors.RetryDelay(new ChatFailure("overloaded", "x"), 0, Now));
        Assert.NotNull(ProviderErrors.RetryDelay(new ChatFailure("server", "x"), 0, Now));
        foreach (var code in new[] { "auth", "quota", "context", "model", "forbidden", "bad_request", "offline", "timeout", "budget" })
            Assert.Null(ProviderErrors.RetryDelay(new ChatFailure(code, "x"), 0, Now));
    }

    [Fact]
    public void Low_quota_warning_below_ten_percent()
    {
        using var r = new HttpResponseMessage();
        Assert.Null(ProviderErrors.LowQuotaWarning(r.Headers));
        r.Headers.TryAddWithoutValidation("x-ratelimit-limit-requests", "50");
        r.Headers.TryAddWithoutValidation("x-ratelimit-remaining-requests", "5");
        Assert.Null(ProviderErrors.LowQuotaWarning(r.Headers));   // exactly 10% left
        using var c = new HttpResponseMessage();
        c.Headers.TryAddWithoutValidation("anthropic-ratelimit-tokens-limit", "100000");
        c.Headers.TryAddWithoutValidation("anthropic-ratelimit-tokens-remaining", "1200");
        Assert.Contains("tokens left", ProviderErrors.LowQuotaWarning(c.Headers));
    }

    [Fact]
    public void Budget_check_and_warning()
    {
        Assert.Null(AiBudget.Check(new AiUsage("d", 199, 0, 200, 0)));
        Assert.Equal("budget", AiBudget.Check(new AiUsage("d", 200, 0, 200, 0))!.Code);
        Assert.Equal("budget", AiBudget.Check(new AiUsage("d", 0, 500, 0, 500))!.Code);
        Assert.Null(AiBudget.Check(new AiUsage("d", 10_000, 10_000_000, 0, 0)));   // 0 = no limit
        Assert.Null(AiBudget.Warning(new AiUsage("d", 159, 0, 200, 0)));
        Assert.NotNull(AiBudget.Warning(new AiUsage("d", 160, 0, 200, 0)));
        Assert.NotNull(AiBudget.Warning(new AiUsage("d", 0, 400, 0, 500)));
        Assert.Null(AiBudget.Warning(new AiUsage("d", 5, 5, 0, 0)));
    }
}

public sealed class ProviderHttpTests
{
    private readonly ScriptHandler _h = new();
    private readonly List<TimeSpan> _delays = [];
    private readonly DateTimeOffset _now = new(2026, 11, 12, 10, 0, 0, TimeSpan.Zero);

    private ProviderHttp Http() => new(_h, () => _now, (t, _) => { _delays.Add(t); return Task.CompletedTask; });

    private Task<HttpResponseMessage> Send(List<ChatFailure>? retries = null) =>
        Http().SendAsync(() => new HttpRequestMessage(HttpMethod.Post, "https://provider.test/x"), "https://provider.test", "m", "k", (f, _) => retries?.Add(f), CancellationToken.None);

    [Fact]
    public async Task Retries_twice_then_fails_with_retry_time()
    {
        for (int i = 0; i < 3; i++) _h.Responses.Enqueue(ScriptHandler.Status(429, "", h => h.RetryAfter = new RetryConditionHeaderValue(TimeSpan.FromSeconds(3))));
        var retries = new List<ChatFailure>();
        var ex = await Assert.ThrowsAsync<ChatFailureException>(() => Send(retries));
        Assert.Equal("rate", ex.Failure.Code);
        Assert.Equal(_now.AddSeconds(3), ex.Failure.RetryAt);
        Assert.Equal(3, _h.Requests.Count);
        Assert.Equal([TimeSpan.FromSeconds(3), TimeSpan.FromSeconds(3)], _delays);
        Assert.Equal(2, retries.Count);
    }

    [Fact]
    public async Task Server_error_then_success()
    {
        _h.Responses.Enqueue(ScriptHandler.Status(500));
        using var r = await Send();
        Assert.True(r.IsSuccessStatusCode);
        Assert.Equal([TimeSpan.FromSeconds(1)], _delays);
    }

    [Fact]
    public async Task No_retry_for_auth_quota_or_long_waits()
    {
        _h.Responses.Enqueue(ScriptHandler.Status(401));
        Assert.Equal("auth", (await Assert.ThrowsAsync<ChatFailureException>(() => Send())).Failure.Code);
        _h.Responses.Enqueue(ScriptHandler.Status(429, """{"error":{"code":"insufficient_quota","message":"quota"}}"""));
        Assert.Equal("quota", (await Assert.ThrowsAsync<ChatFailureException>(() => Send())).Failure.Code);
        _h.Responses.Enqueue(ScriptHandler.Status(429, "", h => h.RetryAfter = new RetryConditionHeaderValue(TimeSpan.FromMinutes(10))));
        var f = (await Assert.ThrowsAsync<ChatFailureException>(() => Send())).Failure;
        Assert.Equal(_now.AddMinutes(10), f.RetryAt);
        Assert.Equal(3, _h.Requests.Count);
        Assert.Empty(_delays);
    }

    [Fact]
    public async Task Offline_is_reported()
    {
        _h.Offline = true;
        var f = (await Assert.ThrowsAsync<ChatFailureException>(() => Send())).Failure;
        Assert.Equal("offline", f.Code);
        Assert.Contains("provider.test", f.Message);
    }

    [Fact]
    public void Loopback_endpoints()
    {
        Assert.True(ProviderHttp.IsLoopback("http://127.0.0.1:1234/v1"));
        Assert.True(ProviderHttp.IsLoopback("http://localhost:11434"));
        Assert.True(ProviderHttp.IsLoopback("http://[::1]:8080"));
        Assert.False(ProviderHttp.IsLoopback("https://api.openai.com/v1"));
    }
}

public sealed class ChatStoreTests : IDisposable
{
    private readonly TempStore _t = new();
    private static readonly DateTimeOffset T = DateTimeOffset.UtcNow.AddHours(-2);

    public void Dispose() => _t.Dispose();

    private static ChatRecord Rec(string id, int messages, DateTimeOffset start, string transcript = "[]") => new(id, "Title " + id, start, start.AddMinutes(messages), "gemini", "m1", transcript,
        Enumerable.Range(0, messages).Select(i => new ChatMessage(i, start.AddMinutes(i), i % 2 == 0 ? "user" : "assistant", "msg " + i,
            i % 2 == 1 ? [new ChatSource("cap" + i, start, CaptureKind.Screen, "chrome.exe", "t", "example.com", null, null)] : null)).ToList());

    [Fact]
    public void Save_appends_and_reads_back()
    {
        _t.Store.SaveChat(Rec("c1", 2, T));
        _t.Store.SaveChat(Rec("c1", 4, T, "[1,2]"));
        var c = _t.Store.GetChat("c1")!;
        Assert.Equal(4, c.Messages.Count);
        Assert.Equal("[1,2]", c.Transcript);
        Assert.Equal("cap1", c.Messages[1].Sources![0].Id);
        Assert.Equal("example.com", c.Messages[1].Sources![0].Site);
        Assert.Empty(c.Messages[0].Sources ?? []);
        Assert.Null(_t.Store.GetChat("nope"));
    }

    [Fact]
    public void List_rename_delete()
    {
        _t.Store.SaveChat(Rec("old", 2, T));
        _t.Store.SaveChat(Rec("new", 2, T.AddMinutes(30)));
        Assert.Equal(["new", "old"], _t.Store.Chats(10).Select(c => c.Id));
        Assert.Equal(2, _t.Store.Chats(10)[0].Messages);
        Assert.True(_t.Store.RenameChat("old", "  " + new string('x', 200) + " "));
        Assert.Equal(EncryptedStore.ChatTitleMax, _t.Store.GetChat("old")!.Title.Length);
        Assert.False(_t.Store.RenameChat("nope", "x"));
        Assert.Equal(1, _t.Store.DeleteChats("old"));
        Assert.Equal(0, _t.Store.DeleteChats("old"));
        Assert.Equal(1, _t.Store.DeleteChats(null));
        Assert.Empty(_t.Store.Chats(10));
    }

    [Fact]
    public void Time_purge_removes_chats_active_in_the_window()
    {
        _t.Store.SaveChat(Rec("inside", 2, T));
        _t.Store.SaveChat(Rec("outside", 2, T.AddDays(-3)));
        _t.Store.Purge(PurgeScope.Range(T.AddMinutes(-1), T.AddMinutes(5)));
        Assert.Null(_t.Store.GetChat("inside"));
        Assert.NotNull(_t.Store.GetChat("outside"));
        _t.Store.Purge(PurgeScope.Last(TimeSpan.FromDays(4)));
        Assert.Empty(_t.Store.Chats(10));
    }

    [Fact]
    public void Usage_accumulates_per_day_and_survives_reopen()
    {
        Assert.Equal((0, 0L), _t.Store.AiUsage("2026-11-12"));
        _t.Store.AddAiUsage("2026-11-12", 1, 300);
        _t.Store.AddAiUsage("2026-11-12", 2, 50);
        _t.Store.AddAiUsage("2026-11-11", 1, 1);
        _t.Store.SaveChat(Rec("c1", 2, T));
        _t.Reopen();   // migration runs again
        Assert.Equal((3, 350L), _t.Store.AiUsage("2026-11-12"));
        Assert.Equal((1, 1L), _t.Store.AiUsage("2026-11-11"));
        Assert.NotNull(_t.Store.GetChat("c1"));
    }
}

public sealed class ChatServiceTests : IDisposable
{
    private const string Key = "test-key-0123456789abcdef";
    private readonly TempStore _t = new();
    private readonly SettingsStore _settings;
    private readonly ScriptHandler _h = new();
    private readonly ScriptProvider _p = new();
    private readonly FakeEmbedder _emb = new();
    private readonly List<(string Verb, int N, string Sha)> _audit = [];
    private readonly List<TimeSpan> _delays = [];
    private readonly DateTimeOffset _now = DateTimeOffset.UtcNow;
    private DateTimeOffset _clock;
    private bool _locked;
    private IDescribeRedactor? _redactor;
    private ChatService _chat;
    private readonly string _shot, _audio;

    public ChatServiceTests()
    {
        _clock = _now;
        _settings = SettingsStore.Load(_t.Home.Path);
        Set("""{"ai":{"enabled":true,"provider":"gemini"}}""");
        var v = Vec.Seeded(7);
        _shot = _t.Store.AddScreenCapture(Caps.Screen(_now.AddMinutes(-5), "chrome.exe", "Acme invoice for bob@example.com"));
        _t.Store.SetEmbedding(_shot, v, Vec.Info);
        _audio = _t.Store.AddAudioCapture(Caps.Audio(_now.AddMinutes(-4), "Call me on +44 20 7946 0958, password: hunter2"));
        _t.Store.SetEmbedding(_audio, Vec.Near(v, 1), Vec.Info);
        _emb.Map["invoice"] = v;
        _chat = Make();
    }

    public void Dispose() => _t.Dispose();

    private ChatService Make() => new(new ChatDeps
    {
        Settings = () => _settings.Current,
        Store = () => _locked ? null : _t.Store,
        Embedder = _emb,
        Thumbnailer = new FakeThumbs(),
        ApiKey = () => Key,
        Http = new ProviderHttp(_h, () => _clock, (t, _) => { _delays.Add(t); return Task.CompletedTask; }),
        Providers = _ => _p,
        ImageRedactor = _redactor,
        Audit = (verb, n, sha) => _audit.Add((verb, n, sha)),
    });

    private void Set(string json) => Assert.Null(_settings.TryUpdate(JsonDocument.Parse(json).RootElement));

    private async Task<List<ChatEvent>> Send(string? chat, string q)
    {
        var ev = new List<ChatEvent>();
        await _chat.SendAsync(chat, q, ev.Add, CancellationToken.None);
        return ev;
    }

    private async Task<List<ChatEvent>> Approve(string token, params string[] remove)
    {
        var ev = new List<ChatEvent>();
        await _chat.ApproveAsync(token, remove, ev.Add, CancellationToken.None);
        return ev;
    }

    private async Task<List<ChatEvent>> Deny(string token)
    {
        var ev = new List<ChatEvent>();
        await _chat.DenyAsync(token, ev.Add, CancellationToken.None);
        return ev;
    }

    private static string Answer(List<ChatEvent> ev) => string.Concat(ev.Where(e => e.Type == "text").Select(e => e.Text));

    /// <summary>A chat that asked, was allowed (audio removed) and answered.</summary>
    private async Task<string> ApprovedChat()
    {
        _p.Turns.Enqueue(ScriptProvider.Search());
        var ev = await Send(null, "Which invoice did I see?");
        _p.Turns.Enqueue(ScriptProvider.Answer());
        var done = await Approve(ev[^1].Token!, _audio);
        Assert.Equal("done", done[^1].Type);
        return ev[0].ChatId!;
    }

    [Fact]
    public async Task First_share_asks_then_sends_only_what_was_kept()
    {
        _p.Turns.Enqueue(ScriptProvider.Search());
        var ev = await Send(null, "Which invoice did I see?");
        Assert.Equal(["start", "search", "approval"], ev.Select(e => e.Type));
        Assert.Equal("invoice", ev[1].Query);
        var ap = ev[^1];
        Assert.Equal(AiShare.TextImages, ap.Share);
        Assert.Equal(ChatShareGate.ImagesUnavailableNote, ap.Text);   // no screen redactor on this branch
        Assert.Equal(2, ap.Items!.Count);
        Assert.All(ap.Items, i => Assert.Null(i.Image));
        var shown = string.Join("\n", ap.Items.Select(i => i.Title + "\n" + i.Text));
        Assert.Contains("Acme invoice for [redacted]", shown);
        Assert.DoesNotContain("bob@example.com", shown);
        Assert.DoesNotContain("hunter2", shown);
        Assert.DoesNotContain("7946", shown);
        Assert.Single(_h.Bodies);                       // only the question has left the PC so far
        Assert.DoesNotContain("Acme", _h.Bodies[0]);
        Assert.Equal(Key, _h.Requests[0].Headers.GetValues("x-api-key").Single());

        _p.Turns.Enqueue(ScriptProvider.Answer());
        var ev2 = await Approve(ap.Token!, _audio);
        Assert.Equal("start", ev2[0].Type);
        Assert.Equal("It was the Acme invoice [S1].", Answer(ev2));
        Assert.Equal("sources", ev2[^2].Type);
        Assert.Equal(_shot, Assert.Single(ev2[^2].Sources!).Id);
        Assert.Equal("done", ev2[^1].Type);
        Assert.Equal(2, ev2[^1].Usage!.Requests);

        Assert.Equal(2, _h.Bodies.Count);
        Assert.Contains("Acme invoice for [redacted]", _h.Bodies[1]);
        Assert.DoesNotContain("Call me", _h.Bodies[1]);
        Assert.DoesNotContain("bob@example.com", _h.Bodies[1]);
        Assert.All(_h.Bodies, b => Assert.DoesNotContain(Key, b));

        Assert.Equal([("chat gemini", 0), ("chat gemini", 1)], _audit.Select(a => (a.Verb, a.N)));
        Assert.All(_audit, a => Assert.Equal(64, a.Sha.Length));
        var chatId = ev[0].ChatId!;
        var saved = _t.Store.GetChat(chatId)!;
        Assert.Equal(["user", "assistant"], saved.Messages.Select(m => m.Role));
        Assert.Equal("Which invoice did I see?", saved.Messages[0].Text);
        Assert.Equal(_shot, saved.Messages[1].Sources![0].Id);
        Assert.Equal((2, 330L), _t.Store.AiUsage(AiBudget.Today(_now)));
        Assert.True(_chat.Get(chatId)!.Approved);
    }

    [Fact]
    public async Task Same_chat_does_not_ask_again_a_new_chat_does()
    {
        var chatId = await ApprovedChat();
        _p.Turns.Enqueue(ScriptProvider.Search());
        _p.Turns.Enqueue(ScriptProvider.Answer("Same one [S1]."));
        var ev = await Send(chatId, "And when was that?");
        Assert.DoesNotContain(ev, e => e.Type == "approval");
        Assert.Equal("done", ev[^1].Type);
        Assert.Equal("Same one [S1].", Answer(ev));
        Assert.Contains("Which invoice did I see?", _h.Bodies[^1]);   // the saved transcript continues
        Assert.Equal(4, _t.Store.GetChat(chatId)!.Messages.Count);

        _p.Turns.Enqueue(ScriptProvider.Search());
        var other = await Send(null, "Another question");
        Assert.Equal("approval", other[^1].Type);
        Assert.NotEqual(chatId, other[0].ChatId);
    }

    [Fact]
    public async Task Deny_answers_without_captures()
    {
        _p.Turns.Enqueue(ScriptProvider.Search());
        var ev = await Send(null, "Which invoice did I see?");
        _p.Turns.Enqueue(ScriptProvider.Answer("I can't tell without your captures."));
        var ev2 = await Deny(ev[^1].Token!);
        Assert.Equal("done", ev2[^1].Type);
        Assert.DoesNotContain(ev2, e => e.Type == "sources");
        Assert.Contains("did not allow sharing", _h.Bodies[^1]);
        Assert.DoesNotContain("Acme", _h.Bodies[^1]);
        Assert.All(_audit, a => Assert.Equal(0, a.N));
        Assert.False(_chat.Get(ev[0].ChatId!)!.Approved);

        _p.Turns.Enqueue(ScriptProvider.Search());
        Assert.Equal("approval", (await Send(ev[0].ChatId, "Try again"))[^1].Type);   // still asks next time
    }

    [Theory]
    [InlineData(AiShare.Metadata)]
    [InlineData(AiShare.Text)]
    public async Task Share_level_decides_what_the_approval_shows(string share)
    {
        Set($$$"""{"ai":{"share":"{{{share}}}"}}""");
        _p.Turns.Enqueue(ScriptProvider.Search());
        var ap = (await Send(null, "Which invoice?"))[^1];
        Assert.Equal(share, ap.Share);
        Assert.Null(ap.Text);   // images were not asked for, so no "unavailable" note
        var shot = ap.Items!.Single(i => i.Id == _shot);
        Assert.Contains("app: chrome.exe", shot.Text);
        if (share == AiShare.Metadata)
        {
            Assert.Equal("", shot.Title);
            Assert.DoesNotContain("Acme", shot.Text);
        }
        else Assert.Contains("title: Acme invoice for [redacted]", shot.Text);
    }

    [Fact]
    public async Task Approval_at_a_lower_share_level_does_not_cover_a_higher_one()
    {
        Set("""{"ai":{"share":"metadata"}}""");
        _p.Turns.Enqueue(ScriptProvider.Search());
        var ev = await Send(null, "Which invoice?");
        _p.Turns.Enqueue(ScriptProvider.Answer());
        await Approve(ev[^1].Token!);
        Set("""{"ai":{"share":"text"}}""");
        _p.Turns.Enqueue(ScriptProvider.Search());
        Assert.Equal("approval", (await Send(ev[0].ChatId, "More detail?"))[^1].Type);
    }

    [Fact]
    public async Task Images_go_only_through_a_redactor_that_redacts()
    {
        _redactor = new FixedRedactor(true);
        _chat = Make();
        _p.Turns.Enqueue(ScriptProvider.Search());
        var ap = (await Send(null, "Which invoice?"))[^1];
        Assert.Null(ap.Text);
        Assert.Equal(FixedRedactor.Bytes, ap.Items!.Single(i => i.Id == _shot).Image);
        Assert.Null(ap.Items!.Single(i => i.Id == _audio).Image);
        _p.Turns.Enqueue(ScriptProvider.Answer());
        await Approve(ap.Token!);
        Assert.Contains(Convert.ToBase64String(FixedRedactor.Bytes), _h.Bodies[^1]);
        Assert.DoesNotContain("\"AQID\"", _h.Bodies[^1]);   // the unredacted thumbnail bytes

        _redactor = new FixedRedactor(false);
        _chat = Make();
        _p.Turns.Enqueue(ScriptProvider.Search());
        ap = (await Send(null, "Which invoice?"))[^1];
        Assert.All(ap.Items!, i => Assert.Null(i.Image));
    }

    [Fact]
    public async Task Budget_stops_before_sending_and_warns_at_80_percent()
    {
        Set("""{"ai":{"dailyRequests":2}}""");
        _t.Store.AddAiUsage(AiBudget.Today(_now), 2, 0);
        var ev = await Send(null, "Hello?");
        Assert.Equal(["start", "error"], ev.Select(e => e.Type));
        Assert.Equal("budget", ev[^1].Code);
        Assert.Empty(_h.Bodies);
        Assert.Empty(_t.Store.Chats(10));

        Set("""{"ai":{"dailyRequests":5}}""");
        _t.Store.AddAiUsage(AiBudget.Today(_now), 2, 0);   // 4 of 5: 80%
        _p.Turns.Enqueue(ScriptProvider.Answer("Hi."));
        ev = await Send(null, "Hello?");
        var warn = Assert.Single(ev, e => e.Type == "warning");
        Assert.Equal("budget", warn.Code);
        Assert.Equal("done", ev[^1].Type);
        Assert.Equal(5, _t.Store.AiUsage(AiBudget.Today(_now)).Requests);
    }

    [Fact]
    public async Task Retries_warn_and_a_final_rate_limit_says_when()
    {
        _h.Responses.Enqueue(ScriptHandler.Status(429, "", h => h.RetryAfter = new RetryConditionHeaderValue(TimeSpan.FromSeconds(1))));
        _p.Turns.Enqueue(ScriptProvider.Answer("Hi."));
        var ev = await Send(null, "Hello?");
        Assert.Equal("retry", Assert.Single(ev, e => e.Type == "warning").Code);
        Assert.Equal("done", ev[^1].Type);

        for (int i = 0; i < 3; i++) _h.Responses.Enqueue(ScriptHandler.Status(429, "", h => h.RetryAfter = new RetryConditionHeaderValue(TimeSpan.FromSeconds(4))));
        ev = await Send(null, "Hello again?");
        Assert.Equal(2, ev.Count(e => e is { Type: "warning", Code: "retry" }));
        Assert.Equal("rate", ev[^1].Code);
        Assert.Equal(_clock.AddSeconds(4), ev[^1].RetryAt);
        Assert.Equal([TimeSpan.FromSeconds(1), TimeSpan.FromSeconds(4), TimeSpan.FromSeconds(4)], _delays);
        Assert.Single(_t.Store.Chats(10));   // the failed chat was not saved
    }

    [Fact]
    public async Task Auth_error_hides_the_key_and_does_not_retry()
    {
        _h.Responses.Enqueue(ScriptHandler.Status(401, $$$"""{"error":{"message":"Incorrect API key provided: {{{Key}}}"}}"""));
        var ev = await Send(null, "Hello?");
        Assert.Equal("auth", ev[^1].Code);
        Assert.DoesNotContain(Key, ev[^1].Text);
        Assert.Single(_h.Requests);
        Assert.Equal([("chat gemini", 0)], _audit.Select(a => (a.Verb, a.N)));   // the attempt is audited
    }

    [Fact]
    public async Task Offline_and_low_quota()
    {
        _h.Offline = true;
        Assert.Equal("offline", (await Send(null, "Hello?"))[^1].Code);
        _h.Offline = false;
        _h.Responses.Enqueue(ScriptHandler.Status(200, "", h =>
        {
            h.TryAddWithoutValidation("x-ratelimit-limit-requests", "50");
            h.TryAddWithoutValidation("x-ratelimit-remaining-requests", "3");
        }));
        _p.Turns.Enqueue(ScriptProvider.Answer("Hi."));
        var ev = await Send(null, "Hello?");
        Assert.Contains(ev, e => e is { Type: "warning", Code: "ratelimit" });
        Assert.Equal("done", ev[^1].Type);
    }

    [Fact]
    public async Task Failed_turn_leaves_the_chat_as_it_was()
    {
        var chatId = await ApprovedChat();
        var before = _t.Store.GetChat(chatId)!;
        for (int i = 0; i < 3; i++) _h.Responses.Enqueue(ScriptHandler.Status(500));
        var ev = await Send(chatId, "Next?");
        Assert.Equal("server", ev[^1].Code);
        var after = _t.Store.GetChat(chatId)!;
        Assert.Equal(before.Transcript, after.Transcript);
        Assert.Equal(before.Messages.Count, after.Messages.Count);

        _p.Turns.Enqueue(ScriptProvider.Answer("Recovered."));
        Assert.Equal("done", (await Send(chatId, "Next?"))[^1].Type);
        Assert.Equal(4, _t.Store.GetChat(chatId)!.Messages.Count);
    }

    [Fact]
    public async Task Refusal_truncation_and_round_limit()
    {
        _p.Turns.Enqueue(ScriptProvider.Answer("", "refusal"));
        Assert.Equal("refused", (await Send(null, "Hello?"))[^1].Code);

        _p.Turns.Enqueue(ScriptProvider.Answer("Long answer", "max_tokens"));
        var ev = await Send(null, "Hello?");
        Assert.Contains(ev, e => e is { Type: "warning", Code: "truncated" });
        Assert.Equal("done", ev[^1].Type);

        var chatId = await ApprovedChat();
        for (int i = 0; i < ChatService.MaxRounds; i++) _p.Turns.Enqueue(ScriptProvider.Search());
        ev = await Send(chatId, "Keep searching");
        Assert.Contains(ev, e => e is { Type: "warning", Code: "rounds" });
        Assert.Equal("done", ev[^1].Type);
    }

    [Fact]
    public async Task Too_many_calls_in_one_round_are_not_run()
    {
        var chatId = await ApprovedChat();
        _p.Turns.Enqueue(ScriptProvider.Search(calls: ChatService.MaxCallsPerRound + 1));
        _p.Turns.Enqueue(ScriptProvider.Answer());
        var ev = await Send(chatId, "Search a lot");
        Assert.Equal(ChatService.MaxCallsPerRound, ev.Count(e => e.Type == "search"));
        Assert.Contains("Too many searches", _p.Outputs[^1].Text);
    }

    [Fact]
    public async Task Locked_store_sends_nothing()
    {
        _locked = true;
        var ev = await Send(null, "Hello?");
        Assert.Equal("locked", ev[^1].Code);
        Assert.Empty(_h.Bodies);
    }

    [Fact]
    public async Task Question_and_token_checks()
    {
        Assert.Equal("bad_request", (await Send(null, "  "))[^1].Code);
        Assert.Equal("bad_request", (await Send(null, new string('q', ChatService.MaxQuestionChars + 1)))[^1].Code);
        Assert.Equal("not_found", (await Send("c_missing", "Hello?"))[^1].Code);
        Assert.Equal("token", (await Approve("nope"))[^1].Code);

        _p.Turns.Enqueue(ScriptProvider.Search());
        var ap = (await Send(null, "Which invoice?"))[^1];
        _clock += ChatService.ApprovalLifetime + TimeSpan.FromSeconds(1);
        Assert.Equal("token", (await Approve(ap.Token!))[^1].Code);
        Assert.Single(_h.Bodies);
    }

    [Fact]
    public async Task Clear_forgets_approvals_and_pending_tokens()
    {
        var chatId = await ApprovedChat();
        _p.Turns.Enqueue(ScriptProvider.Search());
        var pending = (await Send(null, "Other?"))[^1];
        _chat.Clear();
        Assert.Equal("token", (await Approve(pending.Token!))[^1].Code);
        Assert.False(_chat.Get(chatId)!.Approved);
        _p.Turns.Enqueue(ScriptProvider.Search());
        Assert.Equal("approval", (await Send(chatId, "Again?"))[^1].Type);
    }

    [Fact]
    public async Task History_off_keeps_chats_in_memory_only()
    {
        Set("""{"ai":{"chatHistory":false}}""");
        _p.Turns.Enqueue(ScriptProvider.Answer("Hi."));
        var ev = await Send(null, "Hello?");
        var id = ev[0].ChatId!;
        Assert.Empty(_t.Store.Chats(10));
        Assert.Equal(id, Assert.Single(_chat.List(10)).Id);
        Assert.True(_chat.Rename(id, "Renamed"));
        Assert.Equal("Renamed", _chat.Get(id)!.Chat.Title);
        Assert.Equal(1, _chat.Delete(null));
        Assert.Empty(_chat.List(10));
    }

    [Fact]
    public async Task Model_change_starts_a_fresh_transcript_with_a_recap()
    {
        var chatId = await ApprovedChat();
        Set("""{"ai":{"model":"gemini-other"}}""");
        _p.Turns.Enqueue(ScriptProvider.Answer("Ok."));
        await Send(chatId, "Next question");
        Assert.Contains("Earlier in this chat", _h.Bodies[^1]);
        Assert.Contains("New question: Next question", _h.Bodies[^1]);
        Assert.DoesNotContain("Captures from the user", _h.Bodies[^1]);
        Assert.Equal(4, _t.Store.GetChat(chatId)!.Messages.Count);
    }
}

/// <summary>The chat verb end to end with the real Gemini wire format over a scripted HTTP handler.</summary>
public sealed class ChatVerbTests : IDisposable
{
    private const string Key = "test-key-0123456789abcdef";
    private readonly TempStore _t = new();
    private readonly TempHome _home = new(null);
    private readonly SettingsStore _settings;
    private readonly FakeAgent _agent = new();
    private readonly ScriptHandler _h = new();
    private readonly CommandProcessor _p;
    private readonly ClientSession _ui = new(PipeKind.Ui, 101, @"C:\app\ui\BetterRecall.UI.exe");
    private readonly ApiKeyStore _keys;
    private readonly string _shot;

    public ChatVerbTests()
    {
        _settings = SettingsStore.Load(_home.Path);
        _keys = new ApiKeyStore(_home.Path);
        var emb = new FakeEmbedder();
        _p = new CommandProcessor(new CommandDeps
        {
            Settings = _settings, Store = () => _t.Store, Keys = new FakeKeys(), Agent = _agent, Embedder = emb, Thumbnailer = new FakeThumbs(),
            PurgeAll = _ => Task.FromResult(0), AiKey = _keys, Describer = new CloudDescriber(), ChatHttp = new ProviderHttp(_h),
        });
        var v = Vec.Seeded(9);
        _shot = _t.Store.AddScreenCapture(Caps.Screen(DateTimeOffset.UtcNow.AddMinutes(-3), "chrome.exe", "Acme invoice"));
        _t.Store.SetEmbedding(_shot, v, Vec.Info);
        emb.Map["invoice"] = v;
    }

    public void Dispose()
    {
        _t.Dispose();
        _home.Dispose();
    }

    private void Set(string json) => Assert.Null(_settings.TryUpdate(JsonDocument.Parse(json).RootElement));

    private async Task<(bool Ok, JsonElement Data, string? Error)> Call(string line, ClientSession? s = null)
    {
        var r = await _p.HandleAsync(s ?? _ui, line, CancellationToken.None);
        Assert.Null(r.Stream);
        return Envelope.Parse(r.Line);
    }

    private async Task<List<string>> Stream(string line)
    {
        var r = await _p.HandleAsync(_ui, line, CancellationToken.None);
        Assert.NotNull(r.Stream);
        var lines = new List<string>();
        await r.Stream!(lines.Add);
        Assert.All(lines, l => Assert.DoesNotContain('\n', l));
        return lines;
    }

    private static List<JsonElement> Events(List<string> lines) => lines.Select(l =>
    {
        var e = Envelope.Parse(l);
        Assert.True(e.Ok, e.Error);
        return e.Data;
    }).ToList();

    private const string GeminiSearch = """
        data: {"candidates":[{"content":{"role":"model","parts":[{"functionCall":{"name":"search_history","args":{"query":"invoice"}},"thoughtSignature":"sig1"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":40,"candidatesTokenCount":8}}


        """;

    private const string GeminiAnswer = """
        data: {"candidates":[{"content":{"role":"model","parts":[{"text":"The Acme "}]}}]}

        data: {"candidates":[{"content":{"role":"model","parts":[{"text":"invoice [S1]."}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":90,"candidatesTokenCount":6}}


        """;

    [Fact]
    public async Task Gates_before_any_turn()
    {
        Assert.Equal(CommandProcessor.ErrAiOff, (await Call("chat send {\"text\":\"hi\"}")).Error);
        Set("""{"ai":{"enabled":true,"provider":"gemini"}}""");
        Assert.Equal(CommandProcessor.ErrNoApiKey, (await Call("chat send {\"text\":\"hi\"}")).Error);
        _keys.Set(Key);
        Assert.Equal(CommandProcessor.ErrJson, (await Call("chat send not-json")).Error);
        Assert.Equal(CommandProcessor.UsageChat, (await Call("chat")).Error);
        _agent.Unlocked = false;
        Assert.Equal(CommandProcessor.ErrLocked, (await Call("chat list")).Error);
        Assert.Equal(CommandProcessor.ErrLocked, (await Call("chat send {\"text\":\"hi\"}")).Error);
        Assert.Empty(_h.Requests);

        var pub = new ClientSession(PipeKind.Public, 5, @"C:\x.exe");
        Assert.Equal("Unknown command 'chat'.", (await Call("chat list", pub)).Error);
        Assert.True(CommandProcessor.IsLongRunning(PipeKind.Ui, "chat send {}"));
        Assert.True(CommandProcessor.IsLongRunning(PipeKind.Ui, "chat approve {}"));
        Assert.False(CommandProcessor.IsLongRunning(PipeKind.Ui, "chat list"));
        Assert.False(CommandProcessor.IsLongRunning(PipeKind.Public, "chat send {}"));
    }

    [Fact]
    public async Task Gemini_chat_end_to_end()
    {
        Set("""{"ai":{"enabled":true,"provider":"gemini","share":"text"}}""");
        _keys.Set(Key);
        _h.Responses.Enqueue(ScriptHandler.Sse(GeminiSearch));
        var first = Events(await Stream("chat send {\"text\":\"Which invoice did I open?\"}"));
        Assert.Equal(["start", "search", "approval"], first.Select(e => e.GetProperty("type").GetString()));
        var chatId = first[0].GetProperty("chatId").GetString()!;
        var token = first[^1].GetProperty("token").GetString()!;
        Assert.Equal(_shot, first[^1].GetProperty("items")[0].GetProperty("id").GetString());

        _h.Responses.Enqueue(ScriptHandler.Sse(GeminiAnswer));
        var lines = await Stream("chat approve {\"token\":\"" + token + "\",\"remove\":[]}");
        var second = Events(lines);
        Assert.Equal("done", second[^1].GetProperty("type").GetString());
        Assert.Equal("The Acme invoice [S1].", string.Concat(second.Where(e => e.GetProperty("type").GetString() == "text").Select(e => e.GetProperty("text").GetString())));
        Assert.Equal(_shot, second[^2].GetProperty("sources")[0].GetProperty("id").GetString());

        Assert.Equal(2, _h.Requests.Count);
        Assert.All(_h.Requests, r => Assert.Equal(Key, r.Headers.GetValues("x-goog-api-key").Single()));
        Assert.All(_h.Requests, r => Assert.DoesNotContain(Key, r.RequestUri!.ToString()));
        Assert.Contains("functionResponse", _h.Bodies[1]);
        Assert.Contains("sig1", _h.Bodies[1]);   // the thought signature goes back unchanged
        Assert.Contains("Acme invoice", _h.Bodies[1]);
        Assert.All(lines, l => Assert.DoesNotContain(Key, l));

        var audit = _t.Store.ReadAudit(20);
        Assert.Contains(audit, a => a is { Verb: "chat gemini", ResultCount: 1 });
        Assert.DoesNotContain(audit, a => a.Verb.Contains(Key));

        var list = (await Call("chat list")).Data.GetProperty("items");
        Assert.Equal(chatId, list[0].GetProperty("id").GetString());
        var get = (await Call("chat get " + chatId)).Data;
        Assert.True(get.GetProperty("approved").GetBoolean());
        Assert.Equal(2, get.GetProperty("messages").GetArrayLength());
        Assert.True((await Call("chat rename {\"id\":\"" + chatId + "\",\"title\":\"Invoices\"}")).Ok);
        Assert.Equal("Invoices", (await Call("chat get " + chatId)).Data.GetProperty("chat").GetProperty("title").GetString());
        Assert.True((await Call("chat delete " + chatId)).Ok);
        Assert.Equal(CommandProcessor.ErrNotFound, (await Call("chat get " + chatId)).Error);
        Assert.Contains(_t.Store.ReadAudit(20), a => a is { Verb: "chat delete", ResultCount: 1 });

        var status = (await Call("ai")).Data;
        Assert.Equal(2, status.GetProperty("usage").GetProperty("requests").GetInt32());
        Assert.False(status.GetProperty("local").GetBoolean());
    }

    [Fact]
    public async Task Panic_deletes_saved_chats_and_approvals()
    {
        Set("""{"ai":{"enabled":true,"provider":"gemini","share":"text"}}""");
        _keys.Set(Key);
        _h.Responses.Enqueue(ScriptHandler.Sse(GeminiSearch));
        var token = Events(await Stream("chat send {\"text\":\"Which invoice?\"}"))[^1].GetProperty("token").GetString()!;
        _h.Responses.Enqueue(ScriptHandler.Sse(GeminiAnswer));
        await Stream("chat approve {\"token\":\"" + token + "\"}");
        Assert.Single(_t.Store.Chats(10));
        _agent.Unlocked = false;   // a real panic closes the read gate first
        _agent.RaisePanic();
        Assert.Empty(_t.Store.Chats(10));
    }

    [Fact]
    public async Task Turning_history_off_deletes_saved_chats()
    {
        _t.Store.SaveChat(new ChatRecord("c1", "t", DateTimeOffset.UtcNow, DateTimeOffset.UtcNow, "gemini", "m", "[]",
            [new ChatMessage(0, DateTimeOffset.UtcNow, "user", "q")]));
        Assert.True((await Call("settings set {\"ai\":{\"chatHistory\":false}}")).Ok);
        Assert.Empty(_t.Store.Chats(10));
    }
}
