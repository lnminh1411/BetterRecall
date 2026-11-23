// SPDX-License-Identifier: MIT
// Copyright (c) BetterRecall contributors
using System.Text;
using System.Text.Json;
using System.Text.Json.Nodes;
using BetterRecall.Contracts;
using BetterRecall.UI.Logic;

namespace BetterRecall.UI.Tests;

public class AgentClientTests
{
    [Fact]
    public async Task NoPipe_IsNotRunning()
    {
        await using var c = new AgentClient("BetterRecall.uitest.absent." + Guid.NewGuid().ToString("N"), MockHarness.OwnProcessTrust(), TimeSpan.FromMilliseconds(200));
        var r = await c.SendAsync("ping");
        Assert.False(r.Ok);
        Assert.Equal(AgentErrorKind.NotRunning, r.Kind);
    }

    [Fact]
    public async Task WrongServerImage_IsNotTrusted_AndNothingIsSent()
    {
        await using var server = new FakeLineServer(_ => Envelope.Ok("{\"pong\":true}"));
        var trust = new ServerTrustVerifier(@"C:\Program Files\BetterRecall\BetterRecall.Agent.exe");
        await using var c = new AgentClient(server.Name, trust);
        var r = await c.SendAsync("ping");
        Assert.Equal(AgentErrorKind.NotTrusted, r.Kind);
        await Task.Delay(50);
        lock (server.Received) Assert.Empty(server.Received);
    }

    [Fact]
    public async Task TrustedServer_Works_AndReportsPid()
    {
        await using var server = new FakeLineServer(_ => Envelope.Ok("{\"pong\":true}"));
        await using var c = new AgentClient(server.Name, MockHarness.OwnProcessTrust());
        var r = await c.SendAsync("ping");
        Assert.True(r.Ok);
        Assert.True(r.Data.GetProperty("pong").GetBoolean());
        Assert.Equal(Environment.ProcessId, c.ServerPid);
    }

    [Fact]
    public void InstalledAgentPath_IsParentOfUiFolder()
    {
        var p = ServerTrustVerifier.InstalledAgentPath(@"C:\Program Files\BetterRecall\ui\");
        Assert.Equal(@"C:\Program Files\BetterRecall\BetterRecall.Agent.exe", p);
    }

    [Fact]
    public async Task StoreLocked_IsClassified()
    {
        await using var server = new FakeLineServer(_ => Envelope.Error("Store is locked."));
        await using var c = new AgentClient(server.Name, MockHarness.OwnProcessTrust());
        AgentErrorKind? seen = null;
        c.ResponseClassified += k => seen = k;
        var r = await new AgentApi(c).SearchAsync(new SearchQuery("x"));
        Assert.True(r.IsLocked);
        Assert.Equal(AgentErrorKind.Locked, seen);
    }

    [Fact]
    public async Task ServerClosesWithoutAnswer_IsNotRunning_ThenReconnects()
    {
        int n = 0;
        await using var server = new FakeLineServer(_ => Interlocked.Increment(ref n) == 1 ? null : Envelope.Ok("null"));
        await using var c = new AgentClient(server.Name, MockHarness.OwnProcessTrust());
        Assert.Equal(AgentErrorKind.NotRunning, (await c.SendAsync("ping")).Kind);
        Assert.True((await c.SendAsync("ping")).Ok);
    }

    [Theory]
    [InlineData("not json", false)]
    [InlineData("{\"ok\":true,\"data\":{\"nope\":1}}", true)]
    public async Task Malformed_or_wrong_shape_response_is_protocol_error(string reply, bool viaTimeline)
    {
        await using var server = new FakeLineServer(_ => reply);
        await using var c = new AgentClient(server.Name, MockHarness.OwnProcessTrust());
        var kind = viaTimeline
            ? (await new AgentApi(c).TimelineAsync(DateTimeOffset.UtcNow.AddHours(-1), DateTimeOffset.UtcNow)).Kind
            : (await c.SendAsync("ping")).Kind;
        Assert.Equal(AgentErrorKind.Protocol, kind);
    }

    [Fact]
    public async Task OversizedRequest_IsRejectedLocally()
    {
        await using var server = new FakeLineServer(_ => Envelope.Ok("null"));
        await using var c = new AgentClient(server.Name, MockHarness.OwnProcessTrust());
        var r = await c.SendAsync("search " + new string('x', LineProtocol.MaxUiRequestBytes));
        Assert.False(r.Ok);
        Assert.Equal(LineProtocol.TooLongUiError, r.Error);
        lock (server.Received) Assert.Empty(server.Received);
    }

    [Fact]
    public async Task LineReader_HandlesLargeLines_Crlf_Bom_AndSeveralLinesPerRead()
    {
        var big = new string('a', 200_000);
        var payload = Encoding.UTF8.GetBytes("﻿first\r\n" + big + "\nthird");
        var reader = new LineReader(new MemoryStream(payload));
        Assert.Equal("first", await reader.ReadLineAsync(1 << 20, default));
        Assert.Equal(big, await reader.ReadLineAsync(1 << 20, default));
        Assert.Equal("third", await reader.ReadLineAsync(1 << 20, default));
        Assert.Null(await reader.ReadLineAsync(1 << 20, default));
    }

    [Fact]
    public async Task LineReader_EnforcesLimit()
    {
        var reader = new LineReader(new MemoryStream(Encoding.UTF8.GetBytes(new string('a', 100_000) + "\n")));
        await Assert.ThrowsAsync<LineTooLongException>(() => reader.ReadLineAsync(70_000, default));
    }

    [Theory]
    [InlineData("base64")]
    [InlineData("bytes")]
    public void Thumb_parses_base64_and_bytes_fields(string field)
    {
        var roundTripJson = JsonSerializer.Serialize(new ThumbData("f_1", "image/webp", 320, 180, [9, 8]), ContractsJson.Default.ThumbData);
        Assert.Contains("\"base64\"", roundTripJson);
        Assert.Equal([9, 8], Responses.Thumb(JsonDocument.Parse(roundTripJson).RootElement)!.Bytes);

        var json = JsonDocument.Parse($"{{\"id\":\"f_1\",\"mime\":\"image/webp\",\"width\":320,\"height\":180,\"{field}\":\"AQID\"}}").RootElement;
        var t = Responses.Thumb(json)!;
        Assert.Equal([1, 2, 3], t.Bytes);
        Assert.Equal(320, t.Width);
    }

    [Theory]
    [InlineData("Not found.", "Err_NotFound")]
    [InlineData("Wrong passphrase.", "Err_WrongPassphrase")]
    [InlineData("Unlock required.", "Err_UnlockRequired")]
    [InlineData("Wrong password.", "Err_WrongPassword")]
    [InlineData("Too many attempts. Try again in 900 s.", "Err_TooManyTries")]
    [InlineData(AgentErrors.HelloMissing, "Err_HelloMissing")]
    [InlineData(AgentErrors.SettingsUnreadable, "Err_SettingsUnreadable")]
    [InlineData("Rate limit exceeded.", "Err_Raw")]
    public void ServerError_maps_known_errors_to_resources(string error, string key) =>
        Assert.Equal(key, SearchViewModel.ServerError(error).Key);

    [Fact]
    public void ErrorText_and_FromEnvelope_classify_kinds_and_invalid_key()
    {
        Assert.Equal("Err_NotRunning", SearchViewModel.ErrorText(AgentErrorKind.NotRunning, null).Key);
        Assert.Equal("Err_NotTrusted", SearchViewModel.ErrorText(AgentErrorKind.NotTrusted, null).Key);

        var notOpen = AgentResult.FromEnvelope(false, default, "Store is not open.");
        Assert.Equal(AgentErrorKind.NotOpen, notOpen.Kind);
        Assert.False(notOpen.IsLocked);
        Assert.Equal("Err_NotOpen", SearchViewModel.ErrorText(AgentErrorKind.NotOpen, notOpen.Error).Key);
        Assert.Equal(AgentErrorKind.Locked, AgentResult.FromEnvelope(false, default, "Store is locked.").Kind);

        var inv = SearchViewModel.ServerError("Invalid retention.days.");
        Assert.Equal("Err_InvalidKey", inv.Key);
        Assert.Equal("retention.days", inv.Args[0]);
    }

    [Fact]
    public void Enums_AreLowercaseOnTheWire()
    {
        var json = JsonSerializer.Serialize(new SearchHit("a", DateTimeOffset.UnixEpoch, CaptureKind.Audio, "x", "", 0.5f, "", false, 3), ContractsJson.Default.SearchHit);
        Assert.Contains("\"kind\":\"audio\"", json);
    }

    [Fact]
    public void SearchLine_IsLowercaseJson_WithFilters()
    {
        var from = new DateTimeOffset(2026, 11, 1, 0, 0, 0, TimeSpan.Zero);
        var line = AgentApi.BuildSearchLine(new SearchQuery(" invoice ", 20, from, null, "chrome.exe", CaptureKind.Audio));
        Assert.Equal("search {\"q\":\"invoice\",\"limit\":20,\"from\":\"2026-11-01T00:00:00Z\",\"app\":\"chrome.exe\",\"kind\":[\"audio\"]}", line);
    }

    [Fact]
    public void Password_json_escapes_and_omits_nulls()
    {
        var j = JsonNode.Parse(AgentApi.PasswordJson("a\"b\\c", null))!.AsObject();
        Assert.Equal("a\"b\\c", j["current"]!.GetValue<string>());
        Assert.False(j.ContainsKey("new"));
        Assert.Equal("{\"new\":\"x y\"}", AgentApi.PasswordJson(null, "x y"));
        Assert.DoesNotContain('\n', AgentApi.PasswordJson("line\nbreak", "z"));
    }

    [Fact]
    public async Task PasswordStatus_parses_retry_after_over_pipe()
    {
        await using var server = new FakeLineServer(line =>
            line == "password"
                ? Envelope.Ok("{\"set\":true,\"retryAfterS\":30}")
                : Envelope.Error("unexpected"));
        await using var c = new AgentClient(server.Name, MockHarness.OwnProcessTrust());
        var r = await new AgentApi(c).PasswordStatusAsync();
        Assert.True(r.Ok, r.Error);
        Assert.True(r.Value!.Set);
        Assert.Equal(30, r.Value.RetryAfterS);
    }

    [Fact]
    public async Task AiKey_set_and_clear_lines_and_parse_over_pipe()
    {
        await using var server = new FakeLineServer(line => line switch
        {
            "ai setkey test-key-123" => Envelope.Ok("{\"enabled\":false,\"provider\":\"gemini\",\"model\":\"gemini-2.5-flash\",\"keySet\":true,\"local\":false}"),
            "ai clearkey" => Envelope.Ok("{\"enabled\":false,\"provider\":\"gemini\",\"model\":\"gemini-2.5-flash\",\"keySet\":false,\"local\":false}"),
            _ => Envelope.Error("unexpected"),
        });
        await using var c = new AgentClient(server.Name, MockHarness.OwnProcessTrust());
        var api = new AgentApi(c);
        var set = await api.AiSetKeyAsync("test-key-123");
        Assert.True(set.Ok, set.Error);
        Assert.True(set.Value!.KeySet);
        var clear = await api.AiClearKeyAsync();
        Assert.True(clear.Ok, clear.Error);
        Assert.False(clear.Value!.KeySet);
    }

    [Fact]
    public void Responses_search_parses_replay_id_and_offset()
    {
        const string json = """{"count":1,"truncated":false,"tookMs":2,"results":[{"id":"r_hit","ts":"2026-11-12T10:00:00Z","kind":"replay","app":"game.exe","title":"Boss","score":0.8,"snippet":"","hasThumb":true,"replayId":"r_001","replayOffsetMs":15000}]}""";
        var r = Responses.Search(JsonDocument.Parse(json).RootElement)!;
        var hit = Assert.Single(r.Results);
        Assert.Equal(CaptureKind.Replay, hit.Kind);
        Assert.Equal("r_001", hit.ReplayId);
        Assert.Equal(15000, hit.ReplayOffsetMs);
    }

    [Fact]
    public async Task ExportAsync_sends_request_line_and_parses_reply()
    {
        var from = new DateTimeOffset(2026, 11, 10, 0, 0, 0, TimeSpan.Zero);
        var to = new DateTimeOffset(2026, 11, 12, 0, 0, 0, TimeSpan.Zero);
        await using var server = new FakeLineServer(line =>
            line.StartsWith("export {", StringComparison.Ordinal)
                ? Envelope.Ok("{\"count\":4,\"images\":3,\"bytes\":2048}")
                : Envelope.Error("unexpected"));
        await using var c = new AgentClient(server.Name, MockHarness.OwnProcessTrust());
        var api = new AgentApi(c);

        var dry = await api.ExportAsync(new ExportRequest(from, to, "", DryRun: true));
        Assert.True(dry.Ok, dry.Error);
        Assert.Equal(4, dry.Value!.Count);
        Assert.Equal(3, dry.Value.Images);

        var real = await api.ExportAsync(new ExportRequest(from, to, @"C:\exports\out.zip"));
        Assert.True(real.Ok, real.Error);
        Assert.Equal(4, real.Value!.Count);
        lock (server.Received)
        {
            Assert.Equal(2, server.Received.Count);
            Assert.Contains("\"dryRun\":true", server.Received[0]);
            Assert.Contains("out.zip", server.Received[1]);
        }
    }

    [Fact]
    public void Status_json_round_trips_userPaused_and_unlockNeeds()
    {
        var baseStatus = new AgentStatus(false, true, null, "battery", true, null, new AudioState(false, false), new QueueState(0), "NPU",
            new StoreStats(10, 10000, 0, null, null, 0, 0), 30, UserPaused: true, UnlockNeeds: "password");
        var json = JsonSerializer.Serialize(baseStatus, ContractsJson.Default.AgentStatus);
        Assert.Contains("\"userPaused\":true", json);
        Assert.Contains("\"unlockNeeds\":\"password\"", json);
        var parsed = JsonSerializer.Deserialize(json, ContractsJson.Default.AgentStatus)!;
        Assert.True(parsed.UserPaused);
        Assert.Equal("password", parsed.UnlockNeeds);

        var omitted = json.Replace(",\"userPaused\":true", "").Replace(",\"unlockNeeds\":\"password\"", "");
        var oldParsed = JsonSerializer.Deserialize(omitted, ContractsJson.Default.AgentStatus)!;
        Assert.False(oldParsed.UserPaused);
        Assert.Null(oldParsed.UnlockNeeds);
    }

    [Theory]
    [InlineData("Invalid capture.intervalMs.", "capture.intervalMs")]
    [InlineData("Invalid JSON.", null)]
    [InlineData("Invalid or expired token.", null)]
    [InlineData("Store is locked.", null)]
    public void InvalidKey_Parses(string error, string? key) => Assert.Equal(key, AgentErrors.InvalidKey(error));

    /// <summary>C2: UI client limit matches the 64 KiB UI pipe limit.</summary>
    [Fact]
    public async Task Requests_over_4096_bytes_reach_the_agent()
    {
        await using var server = new FakeLineServer(_ => Envelope.Ok("null"));
        await using var c = new AgentClient(server.Name, MockHarness.OwnProcessTrust());
        var line = AgentApi.ChatSendLine(null, new string('x', 4100) + new string('é', 10));
        Assert.True(Encoding.UTF8.GetByteCount(line) > LineProtocol.MaxRequestBytes);
        Assert.True((await c.SendAsync(line)).Ok);
        lock (server.Received) Assert.Equal(line, Assert.Single(server.Received));
    }

    /// <summary>C2: non-ASCII text goes out as UTF-8 (3 bytes per CJK char) instead of \uXXXX.</summary>
    [Fact]
    public void Cjk_chat_and_search_lines_cost_three_bytes_per_character()
    {
        var question = new string('検', 4000);
        var chat = AgentApi.ChatSendLine(null, question);
        Assert.Contains(question, chat);
        Assert.True(Encoding.UTF8.GetByteCount(chat) < 3 * 4000 + 100);
        Assert.True(Encoding.UTF8.GetByteCount(chat) <= LineProtocol.MaxUiRequestBytes);
        var search = AgentApi.BuildSearchLine(new SearchQuery(new string('日', 1024)));
        Assert.True(Encoding.UTF8.GetByteCount(search) < 3 * 1024 + 100);
        Assert.Contains("\\n", AgentApi.ChatSendLine(null, "a\nb"));   // control characters stay escaped: one line
    }

    private sealed class BlockingDisposableClient : IAgentClient, IAsyncDisposable
    {
        public bool Disposed;
        public CancellationToken Seen;
        public int ServerPid => 0;
        public async Task<AgentResult> SendAsync(string line, CancellationToken ct = default)
        {
            Seen = ct;
            await Task.Delay(Timeout.Infinite, ct);
            return AgentResult.Fail(AgentErrorKind.Server, "unreachable");
        }
        public ValueTask DisposeAsync() { Disposed = true; return ValueTask.CompletedTask; }
    }

    /// <summary>S5: long export runs on a fresh client that cancel stops and disposes.</summary>
    [Fact]
    public async Task Export_runs_on_a_fresh_client_that_cancel_stops_and_disposes()
    {
        var made = new List<BlockingDisposableClient>();
        using var cts = new CancellationTokenSource();
        var run = OwnConnection.RunAsync(() => { var c = new BlockingDisposableClient(); made.Add(c); return c; },
            (api, ct) => api.ReplayExportAsync("r1", @"C:\clips\a.mp4", ct), cts.Token);
        await Task.Delay(50);
        Assert.False(run.IsCompleted);
        cts.Cancel();
        await Assert.ThrowsAnyAsync<OperationCanceledException>(() => run);
        var client = Assert.Single(made);
        Assert.True(client.Disposed);
        Assert.True(client.Seen.IsCancellationRequested);
    }

    /// <summary>S5: main client stays free while an export runs on its own connection.</summary>
    [Fact]
    public async Task Main_client_stays_free_while_an_export_runs_on_its_own_connection()
    {
        await using var h = new MockHarness();
        using var cts = new CancellationTokenSource();
        var blocker = new TaskCompletionSource();
        var run = OwnConnection.RunAsync(() => new AgentClient(h.Server.PipeName, MockHarness.OwnProcessTrust()),
            async (api, ct) => { await blocker.Task.WaitAsync(ct); return await api.Client.SendAsync("ping", ct); }, cts.Token);
        var ping = await h.Client.SendAsync("ping").WaitAsync(TimeSpan.FromSeconds(10));
        Assert.True(ping.Ok);
        blocker.SetResult();
        Assert.True((await run).Ok);
    }

    /// <summary>C20/D30: first request after an agent restart is resent on a fresh connection.</summary>
    [Fact]
    public async Task First_request_after_an_agent_restart_is_sent_again_on_a_fresh_connection()
    {
        await using var server = new FakeLineServer(line => Envelope.Ok("\"" + line.Split(' ')[0] + "\""));
        await using var client = new AgentClient(server.Name, MockHarness.OwnProcessTrust());
        Assert.True((await client.SendAsync("status")).Ok);
        server.Restart();
        var r = await client.SendAsync("timeline 2026-11-15T00:00:00Z 2026-11-16T00:00:00Z");
        Assert.True(r.Ok, r.Error);
        Assert.Equal(2, server.Connections);

        // A verb that changes something is also resent when the stale write failed (the agent never saw it).
        server.Restart();
        Assert.True((await client.SendAsync("purge confirm abc")).Ok);
        Assert.Equal(3, server.Connections);
        lock (server.Received) Assert.Single(server.Received, l => l == "purge confirm abc");
    }

    [Theory]
    [InlineData("status", true)]
    [InlineData("search {\"q\":\"x\"}", true)]
    [InlineData("timeline a b", true)]
    [InlineData("chat get 12", true)]
    [InlineData("settings get", true)]
    [InlineData("history", true)]
    [InlineData("history clear", false)]
    [InlineData("settings set {}", false)]
    [InlineData("purge confirm t", false)]
    [InlineData("unlock secret", false)]
    [InlineData("panic", false)]
    [InlineData("chat delete 1", false)]
    public void Only_reading_verbs_are_resent_after_a_lost_reply(string line, bool readOnly)
        => Assert.Equal(readOnly, AgentClient.IsReadOnly(line));

    /// <summary>D10: start action requires an installed agent executable next to the ui folder.</summary>
    [Fact]
    public void Start_action_needs_an_installed_agent()
    {
        var root = Path.Combine(Path.GetTempPath(), "br-s3l-" + Guid.NewGuid().ToString("N"));
        var ui = Path.Combine(root, "ui");
        Directory.CreateDirectory(ui);
        try
        {
            Assert.Null(AgentStart.AgentPath(ui));
            Assert.False(AgentStart.TryStart(ui));
            var agent = Path.Combine(root, InstallLayout.AgentExe);
            File.WriteAllBytes(agent, []);
            Assert.Equal(agent, AgentStart.AgentPath(ui), ignoreCase: true);
            Assert.True(TestResources.Strings().ContainsKey("Err_NotRunning_Start"));
        }
        finally { Directory.Delete(root, recursive: true); }
    }

    /// <summary>D10: reconnect backoff defaults are capped near 5 s and the loop backs off and caps retries.</summary>
    [Fact]
    public async Task Status_reconnect_backoff_is_capped_near_5_s()
    {
        var defaults = new StatusSubscription("unused", new ServerTrustVerifier(@"C:\nowhere\BetterRecall.Agent.exe"));
        Assert.True(defaults.MaxBackoff <= TimeSpan.FromSeconds(5));
        Assert.True(defaults.MinBackoff <= defaults.MaxBackoff);

        // Exercise the doubling/cap loop against a server that closes immediately after connect.
        await using var server = new FakeLineServer((Func<string, string?>)(_ => null));
        await using var sub = new StatusSubscription(server.Name, new AcceptAllTrust())
        {
            MinBackoff = TimeSpan.FromMilliseconds(25),
            MaxBackoff = TimeSpan.FromMilliseconds(50),
        };
        var fourDisconnects = new TaskCompletionSource(TaskCreationOptions.RunContinuationsAsynchronously);
        int disconnects = 0;
        sub.Disconnected += _ => { if (Interlocked.Increment(ref disconnects) >= 4) fourDisconnects.TrySetResult(); };
        var sw = System.Diagnostics.Stopwatch.StartNew();
        sub.Start();
        await fourDisconnects.Task.WaitAsync(TimeSpan.FromSeconds(5));
        sw.Stop();
        // 3 backoff sleeps: 25 ms + 50 ms + 50 ms = 125 ms nominal; checks the loop sleeps (not spins). The 5 s cap itself is checked on the defaults above.
        Assert.True(sw.ElapsedMilliseconds >= 80, $"Expected backoff sleeps, took only {sw.ElapsedMilliseconds} ms");
    }

    /// <summary>AB-S1: thumbnails load on the media connection while a describe preview blocks on its own connection.</summary>
    [Fact]
    public async Task Thumbnails_load_while_a_describe_preview_waits()
    {
        var previewArrived = new TaskCompletionSource(TaskCreationOptions.RunContinuationsAsynchronously);
        using var hold = new ManualResetEventSlim(false);
        await using var server = new FakeLineServer(line =>
        {
            if (line.StartsWith("describe preview", StringComparison.Ordinal))
            {
                previewArrived.TrySetResult();
                hold.Wait(TimeSpan.FromSeconds(20));
            }
            return line.StartsWith("thumb", StringComparison.Ordinal)
                ? Envelope.Ok("{\"id\":\"s_1\",\"mime\":\"image/webp\",\"width\":2,\"height\":2,\"bytes\":\"AAAA\"}")
                : Envelope.Error(AgentErrors.NoImage);
        });
        await using var media = new AgentClient(server.Name, new AcceptAllTrust());
        await using var session = new DescribeSession(new AgentClient(server.Name, new AcceptAllTrust()));
        var preview = session.PreviewAsync("s_1");
        await previewArrived.Task.WaitAsync(TimeSpan.FromSeconds(5));
        var thumb = await new AgentApi(media).ThumbAsync("s_1").WaitAsync(TimeSpan.FromSeconds(5));
        Assert.True(thumb.Ok, thumb.Error);
        Assert.False(preview.IsCompleted);
        hold.Set();
        await preview.WaitAsync(TimeSpan.FromSeconds(5));
    }

    /// <summary>AB-S1: a slow one-shot search does not block requests on the main connection.</summary>
    [Fact]
    public async Task One_shot_search_does_not_hold_the_main_connection()
    {
        var searchArrived = new TaskCompletionSource(TaskCreationOptions.RunContinuationsAsynchronously);
        using var hold = new ManualResetEventSlim(false);
        await using var server = new FakeLineServer(line =>
        {
            if (line.StartsWith("search", StringComparison.Ordinal))
            {
                searchArrived.TrySetResult();
                hold.Wait(TimeSpan.FromSeconds(20));
                return Envelope.Ok("{\"count\":0,\"truncated\":false,\"tookMs\":1,\"results\":[]}");
            }
            return Envelope.Ok("{\"pong\":true}");
        });
        await using var main = new AgentClient(server.Name, new AcceptAllTrust());
        var oneShot = new OneShotClient(() => new AgentClient(server.Name, new AcceptAllTrust()));
        AgentErrorKind? classified = null;
        oneShot.ResponseClassified += k => classified = k;
        var search = new AgentApi(oneShot).SearchAsync(new SearchQuery("acme"));
        await searchArrived.Task.WaitAsync(TimeSpan.FromSeconds(5));
        Assert.True((await main.SendAsync("ping").WaitAsync(TimeSpan.FromSeconds(5))).Ok);
        Assert.False(search.IsCompleted);
        hold.Set();
        Assert.True((await search.WaitAsync(TimeSpan.FromSeconds(5))).Ok);
        Assert.Equal(AgentErrorKind.None, classified);
    }

    /// <summary>M1: read-only one-shot falls back to the main connection when the pipe is busy; writes never resend.</summary>
    [Fact]
    public async Task One_shot_falls_back_to_the_main_connection_when_the_pipe_is_busy()
    {
        await using var server = new FakeLineServer(_ => Envelope.Ok("{\"count\":0,\"truncated\":false,\"tookMs\":1,\"results\":[]}"));
        await using var main = new AgentClient(server.Name, new AcceptAllTrust());
        var busy = new OneShotClient(() => new AgentClient("BetterRecall.nobody." + Guid.NewGuid().ToString("N"), new AcceptAllTrust(), TimeSpan.FromMilliseconds(100)), main);
        AgentErrorKind? classified = null;
        busy.ResponseClassified += k => classified = k;
        Assert.True((await new AgentApi(busy).SearchAsync(new SearchQuery("acme"))).Ok);   // read-only: answered on main
        Assert.Null(classified);   // no "agent not running" from the one-shot side
        var write = await busy.SendAsync("pause");
        Assert.Equal(AgentErrorKind.NotRunning, write.Kind);   // a change is never sent twice
    }
}
