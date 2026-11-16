// SPDX-License-Identifier: MIT
// Copyright (c) BetterRecall contributors
using System.Text.Json;
using BetterRecall.Contracts;
using BetterRecall.UI.Logic;
using Xunit;

namespace BetterRecall.UI.Tests;

/// <summary>K review S2: "starting, try again" only for a cold-start timeout; a failed visual search says it is
/// unavailable.</summary>
public sealed class Sweep3KUiTests
{
    private sealed class FixedClient(string data) : IAgentClient
    {
        public int ServerPid => 1;
        public Task<AgentResult> SendAsync(string line, CancellationToken ct = default) => Task.FromResult(AgentClient.Parse(Envelope.Ok(data)));
    }

    private static Dictionary<string, string> Strings()
    {
        var dir = new DirectoryInfo(AppContext.BaseDirectory);
        while (dir is not null && !File.Exists(Path.Combine(dir.FullName, "BetterRecall.slnx"))) dir = dir.Parent;
        Assert.NotNull(dir);
        var resw = System.Xml.Linq.XDocument.Load(Path.Combine(dir!.FullName, "src", "BetterRecall.UI", "Strings", "en-US", "Resources.resw"));
        return resw.Root!.Elements("data").ToDictionary(e => (string)e.Attribute("name")!, e => (string)e.Element("value")!);
    }

    [Theory]
    [InlineData(null, 0, "Search_NoResults")]
    [InlineData(null, 1, "Search_Count")]
    [InlineData("vector", 0, "Search_NoResultsTextOnly")]
    [InlineData("vector", 1, "Search_CountTextOnly")]
    [InlineData("vector-unavailable", 0, "Search_NoResultsVisualUnavailable")]
    [InlineData("vector-unavailable", 1, "Search_CountVisualUnavailable")]
    public async Task Degraded_search_message_matches_the_cause(string? degraded, int hits, string key)
    {
        SearchHit[] results = hits == 0 ? [] :
            [new("a1", DateTimeOffset.UtcNow, CaptureKind.Screen, "chrome.exe", "t", 0.5f, "", true, null, 0.5f, true, false, "")];
        var data = JsonSerializer.Serialize(new SearchResponse(hits, false, 3, results, degraded), ContractsJson.Default.SearchResponse);
        var vm = new SearchViewModel(new AgentApi(new FixedClient(data))) { Query = "acme" };
        await vm.SearchNowAsync();
        Assert.Equal(key, vm.Message.Key);
        var text = Strings()[key];
        if (degraded == "vector-unavailable") Assert.DoesNotContain("starting", text);
    }
}
