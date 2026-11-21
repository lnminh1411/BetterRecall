// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Text.Json;
using BetterRecall.Agent.Conformance.Harness;
using BetterRecall.Contracts;

namespace BetterRecall.Agent.Conformance;

[Collection("AgentConformance")]
public sealed class McpTests(InstallFixture install)
{
    [Fact]
    public async Task Mcp_lists_tools_and_reports_disabled_when_mcp_is_off()
    {
        await using var h = new AgentHarness(install);
        await h.StartAndConnectUiAsync();

        await using var mcp = h.StartMcp();
        var init = await mcp.InitializeAsync();
        var initResult = init.GetProperty("result");
        Assert.Equal("2025-11-25", initResult.GetProperty("protocolVersion").GetString());
        Assert.Equal("betterrecall", initResult.GetProperty("serverInfo").GetProperty("name").GetString());

        var toolsResp = await mcp.RequestAsync("tools/list");
        var toolNames = toolsResp.GetProperty("result").GetProperty("tools")
            .EnumerateArray()
            .Select(t => t.GetProperty("name").GetString())
            .ToList();
        Assert.Equal(["search_history", "get_capture", "recall_status"], toolNames);

        var statusCall = await mcp.RequestAsync("tools/call", new { name = "recall_status", arguments = new { } });
        var statusResult = statusCall.GetProperty("result");
        Assert.True(statusResult.GetProperty("isError").GetBoolean());
        Assert.Equal("BetterRecall not running, or MCP is off (setting mcp.enabled).",
            statusResult.GetProperty("content")[0].GetProperty("text").GetString());
    }

    [Fact]
    public async Task Mcp_answers_recall_status_and_search_on_empty_store()
    {
        await using var h = new AgentHarness(install, s => s.Mcp.Enabled = true);
        await h.StartAndConnectUiAsync();
        await h.SetupPassphraseAndWaitStoreAsync();
        await h.WaitForWorkerWarmAsync();

        await using var mcp = h.StartMcp();
        await mcp.InitializeAsync();

        var statusCall = await mcp.RequestAsync("tools/call", new { name = "recall_status", arguments = new { } });
        var statusResult = statusCall.GetProperty("result");
        Assert.False(statusResult.GetProperty("isError").GetBoolean());
        var statusJson = statusResult.GetProperty("content")[0].GetProperty("text").GetString()!;
        var st = JsonSerializer.Deserialize(statusJson, ContractsJson.Default.AgentStatus)!;
        Assert.False(st.Locked);
        Assert.Equal(0, st.Store.Rows);

        var searchCall = await mcp.RequestAsync("tools/call", new
        {
            name = "search_history",
            arguments = new { q = "quarterly", limit = 5 },
        });
        var searchResult = searchCall.GetProperty("result");
        Assert.False(searchResult.GetProperty("isError").GetBoolean());
        var searchJson = searchResult.GetProperty("content")[0].GetProperty("text").GetString()!;
        var search = JsonSerializer.Deserialize(searchJson, ContractsJson.Default.SearchResponse)!;
        Assert.Equal(0, search.Count);
        Assert.False(search.Truncated);
        Assert.Empty(search.Results);
    }

    [Fact]
    public async Task Mcp_search_respects_locked_gate()
    {
        await using var h = new AgentHarness(install, s => s.Mcp.Enabled = true);
        await h.StartAndConnectUiAsync();
        await h.SetupPassphraseAndWaitStoreAsync();
        await h.SetUnlockPasswordAsync();
        Assert.True((await h.Ui.RequestAsync("ui", "lock")).Ok);

        await using var mcp = h.StartMcp();
        await mcp.InitializeAsync();

        var searchCall = await mcp.RequestAsync("tools/call", new
        {
            name = "search_history",
            arguments = new { q = "quarterly" },
        });
        var result = searchCall.GetProperty("result");
        Assert.True(result.GetProperty("isError").GetBoolean());
        Assert.Equal("Store is locked.", result.GetProperty("content")[0].GetProperty("text").GetString());
    }
}
