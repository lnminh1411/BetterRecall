// SPDX-License-Identifier: MIT
// Copyright (c) BetterRecall contributors
using System.Text.Json;
using BetterRecall.Contracts;
using BetterRecall.UI.Logic;

namespace BetterRecall.UI.Tests;

public sealed class PurgeFlowTests
{
    /// <summary>D9: purge asks for a fresh token after the confirm dialog and confirms again if more captures match.</summary>
    [Theory]
    [InlineData(new[] { 5, 5 }, new[] { 5 }, "purge confirm t2")]
    [InlineData(new[] { 5, 7, 7 }, new[] { 5, 7 }, "purge confirm t3")]
    public async Task Purge_asks_fresh_token_after_confirm_and_reconfirms_when_count_grows(int[] counts, int[] expectedShown, string expectedLine)
    {
        var client = ScriptedClient.FixedOk("{\"purged\":5}");
        int asks = 0;
        var shown = new List<int>();
        var r = await PurgeFlow.ConfirmFreshAsync(new AgentApi(client),
            () =>
            {
                var n = counts[asks];
                asks++;
                return Task.FromResult(new AgentResult<PurgeConfirmReply>(true, new PurgeConfirmReply("t" + asks, n), AgentErrorKind.None, null));
            },
            rows => { shown.Add(rows); return Task.FromResult(true); });
        Assert.True(r!.Value.Ok);
        Assert.Equal(expectedShown, shown);
        Assert.Equal(expectedLine, Assert.Single(client.Lines));
    }

    [Fact]
    public async Task Purge_cancel_sends_nothing()
    {
        var client = ScriptedClient.FixedOk("{\"purged\":5}");
        var r = await PurgeFlow.ConfirmFreshAsync(new AgentApi(client),
            () => Task.FromResult(new AgentResult<PurgeConfirmReply>(true, new PurgeConfirmReply("t", 3), AgentErrorKind.None, null)),
            _ => Task.FromResult(false));
        Assert.Null(r);
        Assert.Empty(client.Lines);
    }

    /// <summary>P: purge all reports replay files left undeleted for the next start.</summary>
    [Fact]
    public void Purge_all_reports_replay_files_left_for_the_next_start()
    {
        var r = Responses.PurgeResult(JsonDocument.Parse("{\"purged\":12,\"undeleted\":2}").RootElement);
        Assert.Equal(new PurgeResult(12, 2), r);
        Assert.Equal(["Danger_AllDone", "Danger_AllUndeleted"], PurgeFlow.AllDoneText(r).Select(t => t.Key));
        Assert.Equal(["Danger_AllDone"], PurgeFlow.AllDoneText(Responses.PurgeResult(JsonDocument.Parse("{\"purged\":3}").RootElement)).Select(t => t.Key));
        Assert.Equal(["Danger_AllDone"], PurgeFlow.AllDoneText(new PurgeResult(3, 0)).Select(t => t.Key));
    }

    /// <summary>B2: PurgeAll proceeds when empty unlock hits HelloMissing, and aborts on UnlockCancelled.</summary>
    [Fact]
    public async Task Purge_flow_proceeds_when_hello_missing_on_empty_unlock_and_aborts_on_hello_cancel()
    {
        await using var noHelloSrv = new FakeLineServer(line => line switch
        {
            "unlock" => Envelope.Error(AgentErrors.HelloMissing),
            "purge all" => Envelope.Ok("{\"confirm\":\"tok-1\"}"),
            "purge confirm tok-1" => Envelope.Ok("{\"purged\":5}"),
            _ => Envelope.Error("unexpected"),
        });
        await using (var c = new AgentClient(noHelloSrv.Name, MockHarness.OwnProcessTrust()))
        {
            var r = await PurgeFlow.PurgeAllAsync(new AgentApi(c), null);
            Assert.True(r.Ok, r.Error);
            Assert.Equal(5, r.Value!.Purged);
        }

        await using var cancelSrv = new FakeLineServer(line => line switch
        {
            "unlock" => Envelope.Error("Unlock cancelled."),
            _ => Envelope.Error("should not be reached"),
        });
        await using (var c = new AgentClient(cancelSrv.Name, MockHarness.OwnProcessTrust()))
        {
            var r = await PurgeFlow.PurgeAllAsync(new AgentApi(c), null);
            Assert.False(r.Ok);
            Assert.Equal("Unlock cancelled.", r.Error);
            Assert.Single(cancelSrv.Received);
        }
    }
}
