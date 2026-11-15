// SPDX-License-Identifier: MIT
// Copyright (c) BetterRecall contributors
using System.Text;
using System.Text.Json.Nodes;
using BetterRecall.Contracts;
using BetterRecall.UI.Logic;
using BetterRecall.UI.Logic.Mock;

namespace BetterRecall.UI.Tests;

/// <summary>Bug sweep 2, batch H (UI client side): request limit, compact JSON, exports on their own connection, PID reuse.</summary>
public class BatchHCoreUiTests
{
    // ---------------------------------------------------------------- C2: UI client limit matches the UI pipe

    [Fact]
    public async Task C2_Requests_over_4096_bytes_reach_the_agent()
    {
        await using var server = new FakeLineServer(_ => Envelope.Ok("null"));
        await using var c = new AgentClient(server.Name, MockHarness.OwnProcessTrust());
        var line = AgentApi.ChatSendLine(null, new string('x', 4100) + new string('é', 10));
        Assert.True(Encoding.UTF8.GetByteCount(line) > LineProtocol.MaxRequestBytes);
        Assert.True((await c.SendAsync(line)).Ok);
        lock (server.Received) Assert.Equal(line, Assert.Single(server.Received));
    }

    [Fact]
    public void C2_Cjk_chat_and_search_lines_cost_three_bytes_per_character()
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

    [Fact]
    public void C2_Settings_diff_uses_the_ui_limit()
    {
        var original = new JsonObject { ["privacy"] = new JsonObject { ["denyProcesses"] = new JsonArray() } };
        var edited = (JsonObject)original.DeepClone();
        edited["privacy"]!["denyProcesses"] = new JsonArray(Enumerable.Range(0, 1000).Select(i => (JsonNode)$"process{i:D4}.exe").ToArray());
        var line = SettingsDiff.ToRequestLine(SettingsDiff.Diff(original, edited))!;
        Assert.True(Encoding.UTF8.GetByteCount(line) > LineProtocol.MaxRequestBytes);
    }

    // ---------------------------------------------------------------- S5: long exports on their own connection

    private sealed class RecordingClient : IAgentClient, IAsyncDisposable
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

    [Fact]
    public async Task S5_Export_runs_on_a_fresh_client_that_cancel_stops_and_disposes()
    {
        var made = new List<RecordingClient>();
        using var cts = new CancellationTokenSource();
        var run = OwnConnection.RunAsync(() => { var c = new RecordingClient(); made.Add(c); return c; },
            (api, ct) => api.ReplayExportAsync("r1", @"C:\clips\a.mp4", ct), cts.Token);
        await Task.Delay(50);
        Assert.False(run.IsCompleted);
        cts.Cancel();
        await Assert.ThrowsAnyAsync<OperationCanceledException>(() => run);
        var client = Assert.Single(made);
        Assert.True(client.Disposed);
        Assert.True(client.Seen.IsCancellationRequested);
    }

    [Fact]
    public async Task S5_Main_client_stays_free_while_an_export_runs_on_its_own_connection()
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

    // ---------------------------------------------------------------- B14: PID reuse on the client side

    [Fact]
    public async Task B14_Server_process_newer_than_the_connection_is_not_trusted()
    {
        await using var server = new FakeLineServer(_ => Envelope.Ok("null"));
        var trust = new ServerTrustVerifier(Environment.ProcessPath!, startedOf: _ => DateTime.UtcNow.AddMinutes(5));
        await using var c = new AgentClient(server.Name, trust);
        Assert.Equal(AgentErrorKind.NotTrusted, (await c.SendAsync("ping")).Kind);
        lock (server.Received) Assert.Empty(server.Received);
    }

    [Fact]
    public async Task B14_Real_start_time_check_trusts_a_long_running_server()
    {
        await using var server = new FakeLineServer(_ => Envelope.Ok("null"));
        await using var c = new AgentClient(server.Name, new ServerTrustVerifier(Environment.ProcessPath!));   // default: one handle, start time checked
        Assert.True((await c.SendAsync("ping")).Ok);
    }
}
