// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Text.Json;
using BetterRecall.Agent.Conformance.Harness;
using BetterRecall.Contracts;

namespace BetterRecall.Agent.Conformance;

[Collection("AgentConformance")]
public sealed class EmptyStoreTests(InstallFixture install)
{
    [Fact]
    public async Task Search_on_empty_store_returns_empty_results_across_modes()
    {
        await using var h = new AgentHarness(install);
        await h.StartAndConnectUiAsync();
        await h.SetupPassphraseAndWaitStoreAsync();
        await h.WaitForWorkerWarmAsync();

        foreach (var cmd in new[]
        {
            "search acme",
            "search {\"q\":\"acme\",\"limit\":10}",
            "search {\"q\":\"acme\",\"mode\":\"text\"}",
            "search {\"q\":\"acme\",\"mode\":\"vector\"}",
        })
        {
            var (ok, d, err) = await h.Ui.RequestAsync("ui", cmd);
            Assert.True(ok, $"{cmd}: {err}");
            var resp = d.Deserialize(ContractsJson.Default.SearchResponse)!;
            Assert.Equal(0, resp.Count);
            Assert.False(resp.Truncated);
            Assert.Empty(resp.Results);
            Assert.True(resp.TookMs >= 0);
            Assert.Null(resp.Degraded);
        }
    }

    [Fact]
    public async Task Timeline_facets_and_replay_list_are_empty_on_fresh_store()
    {
        await using var h = new AgentHarness(install);
        await h.StartAndConnectUiAsync();
        await h.SetupPassphraseAndWaitStoreAsync();

        var (tlOk, tlData, tlErr) = await h.Ui.RequestAsync("ui", "timeline 2020-01-01T00:00:00Z 2030-01-01T00:00:00Z 10");
        Assert.True(tlOk, tlErr);
        var tl = tlData.Deserialize(ContractsJson.Default.ItemsReplyCaptureMeta)!;
        Assert.Empty(tl.Items);

        var (fOk, fData, fErr) = await h.Ui.RequestAsync("ui", "facets");
        Assert.True(fOk, fErr);
        var facets = fData.Deserialize(ContractsJson.Default.FacetsReply)!;
        Assert.Empty(facets.Apps);
        Assert.Empty(facets.Sites);

        var (rOk, rData, rErr) = await h.Ui.RequestAsync("ui", "replay list");
        Assert.True(rOk, rErr);
        var replays = rData.Deserialize(ContractsJson.Default.ItemsReplyReplayClip)!;
        Assert.Empty(replays.Items);
    }

    [Fact]
    public async Task Stats_and_audit_shape_on_empty_store()
    {
        await using var h = new AgentHarness(install);
        await h.StartAndConnectUiAsync();
        await h.SetupPassphraseAndWaitStoreAsync();

        var (stOk, stData, stErr) = await h.Ui.RequestAsync("ui", "status");
        Assert.True(stOk, stErr);
        var st = stData.Deserialize(ContractsJson.Default.AgentStatus)!;
        Assert.Equal(0, st.Store.Rows);
        Assert.Equal(0, st.Store.ThumbBytes);
        Assert.Null(st.Store.Oldest);
        Assert.Null(st.Store.Newest);
        Assert.Equal(0, st.Store.PendingEmbeddings);
        Assert.Equal(0, st.Store.Vectors);
        Assert.True(st.Store.Bytes >= 0); // the agent reports 0 right after setup

        var (aOk, aData, aErr) = await h.Ui.RequestAsync("ui", "audit 20");
        Assert.True(aOk, aErr);
        var audit = aData.Deserialize(ContractsJson.Default.ItemsReplyAuditEntry)!;
        Assert.NotEmpty(audit.Items);
        Assert.All(audit.Items, entry =>
        {
            Assert.False(string.IsNullOrWhiteSpace(entry.Pipe));
            Assert.False(string.IsNullOrWhiteSpace(entry.Verb));
            Assert.True(entry.ClientPid > 0);
        });
    }

    [Fact]
    public async Task History_add_list_delete_and_clear_on_empty_store()
    {
        await using var h = new AgentHarness(install);
        await h.StartAndConnectUiAsync();
        await h.SetupPassphraseAndWaitStoreAsync();

        var (h0Ok, h0Data, _) = await h.Ui.RequestAsync("ui", "history");
        Assert.True(h0Ok);
        Assert.Empty(h0Data.Deserialize(ContractsJson.Default.ItemsReplyHistoryEntry)!.Items);

        Assert.True((await h.Ui.RequestAsync("ui", "history add acme invoice")).Ok);
        var (h1Ok, h1Data, _) = await h.Ui.RequestAsync("ui", "history");
        Assert.True(h1Ok);
        var item = Assert.Single(h1Data.Deserialize(ContractsJson.Default.ItemsReplyHistoryEntry)!.Items);
        Assert.Equal("acme invoice", item.Q);

        Assert.True((await h.Ui.RequestAsync("ui", "history delete acme invoice")).Ok);
        var (h2Ok, h2Data, _) = await h.Ui.RequestAsync("ui", "history");
        Assert.True(h2Ok);
        Assert.Empty(h2Data.Deserialize(ContractsJson.Default.ItemsReplyHistoryEntry)!.Items);

        Assert.True((await h.Ui.RequestAsync("ui", "history add standup")).Ok);
        Assert.True((await h.Ui.RequestAsync("ui", "history clear")).Ok);
        var (h3Ok, h3Data, _) = await h.Ui.RequestAsync("ui", "history");
        Assert.True(h3Ok);
        Assert.Empty(h3Data.Deserialize(ContractsJson.Default.ItemsReplyHistoryEntry)!.Items);
    }

    [Fact]
    public async Task Get_thumb_and_keyframe_missing_id_return_not_found()
    {
        await using var h = new AgentHarness(install);
        await h.StartAndConnectUiAsync();
        await h.SetupPassphraseAndWaitStoreAsync();

        foreach (var cmd in new[] { "get s_missing", "thumb s_missing", "keyframe s_missing" })
        {
            var (ok, _, err) = await h.Ui.RequestAsync("ui", cmd);
            Assert.False(ok);
            Assert.Equal("Not found.", err);
        }
    }
}
