// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Text.Json;
using BetterRecall.Mcp;

namespace BetterRecall.Core.Tests;

public class McpTests
{
    private readonly List<string> _sent = [];

    private McpServer Server(bool ok = true, string text = "{\"pong\":true}")
        => new((line, _) => { _sent.Add(line); return Task.FromResult(new PipeAnswer(ok, text)); });

    private static JsonElement J(string? line) => JsonDocument.Parse(line!).RootElement.Clone();

    [Theory]
    [InlineData("2025-11-25", "2025-11-25")]
    [InlineData("2025-06-18", "2025-06-18")]
    [InlineData("2024-11-05", "2024-11-05")]
    [InlineData("2099-01-01", "2025-11-25")]
    public async Task Legacy_initialize_negotiates(string asked, string got)
    {
        var r = J(await Server().HandleAsync($"{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{{\"protocolVersion\":\"{asked}\",\"capabilities\":{{}},\"clientInfo\":{{\"name\":\"t\",\"version\":\"1\"}}}}}}", default));
        Assert.Equal(got, r.GetProperty("result").GetProperty("protocolVersion").GetString());
        Assert.False(r.GetProperty("result").GetProperty("capabilities").GetProperty("tools").GetProperty("listChanged").GetBoolean());
        Assert.Equal("betterrecall", r.GetProperty("result").GetProperty("serverInfo").GetProperty("name").GetString());
    }

    [Fact]
    public async Task Notifications_get_no_reply_and_parse_errors_do()
    {
        var s = Server();
        Assert.Null(await s.HandleAsync("{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}", default));
        Assert.Equal(-32700, J(await s.HandleAsync("{nope", default)).GetProperty("error").GetProperty("code").GetInt32());
        Assert.Equal(-32601, J(await s.HandleAsync("{\"jsonrpc\":\"2.0\",\"id\":\"a\",\"method\":\"resources/list\"}", default)).GetProperty("error").GetProperty("code").GetInt32());
    }

    [Fact]
    public async Task Tools_list_is_one_line_with_three_tools()
    {
        var line = await Server().HandleAsync("{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/list\"}", default);
        Assert.NotNull(line);
        Assert.DoesNotContain('\n', line);
        var tools = J(line).GetProperty("result").GetProperty("tools").EnumerateArray().Select(t => t.GetProperty("name").GetString()).ToList();
        Assert.Equal(["search_history", "get_capture", "recall_status"], tools);
    }

    [Fact]
    public async Task Tool_calls_map_to_pipe_verbs()
    {
        var s = Server(text: "{\"count\":0}");
        var r = J(await s.HandleAsync("{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"tools/call\",\"params\":{\"name\":\"search_history\",\"arguments\":{\"q\":\"acme\",\"limit\":5,\"evil\":\"x\"}}}", default));
        Assert.Equal("search {\"q\":\"acme\",\"limit\":5}", _sent[^1]);
        Assert.False(r.GetProperty("result").GetProperty("isError").GetBoolean());
        Assert.Equal("{\"count\":0}", r.GetProperty("result").GetProperty("content")[0].GetProperty("text").GetString());
        await s.HandleAsync("{\"jsonrpc\":\"2.0\",\"id\":4,\"method\":\"tools/call\",\"params\":{\"name\":\"get_capture\",\"arguments\":{\"id\":\"s_01ABC\"}}}", default);
        Assert.Equal("get s_01ABC", _sent[^1]);
        await s.HandleAsync("{\"jsonrpc\":\"2.0\",\"id\":5,\"method\":\"tools/call\",\"params\":{\"name\":\"recall_status\"}}", default);
        Assert.Equal("status", _sent[^1]);
        // An id with a space could smuggle a second verb argument: refused before the pipe.
        var bad = J(await s.HandleAsync("{\"jsonrpc\":\"2.0\",\"id\":6,\"method\":\"tools/call\",\"params\":{\"name\":\"get_capture\",\"arguments\":{\"id\":\"a b\"}}}", default));
        Assert.True(bad.GetProperty("result").GetProperty("isError").GetBoolean());
        Assert.Equal(3, _sent.Count);
        Assert.Equal(-32602, J(await s.HandleAsync("{\"jsonrpc\":\"2.0\",\"id\":7,\"method\":\"tools/call\",\"params\":{\"name\":\"purge\"}}", default)).GetProperty("error").GetProperty("code").GetInt32());
    }

    [Fact]
    public async Task Pipe_errors_become_tool_errors()
    {
        var r = J(await Server(false, "Store is locked.").HandleAsync("{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/call\",\"params\":{\"name\":\"recall_status\",\"arguments\":{}}}", default));
        Assert.True(r.GetProperty("result").GetProperty("isError").GetBoolean());
        Assert.Equal("Store is locked.", r.GetProperty("result").GetProperty("content")[0].GetProperty("text").GetString());
    }

    [Fact]
    public async Task Modern_requests_and_unsupported_version()
    {
        var s = Server();
        const string meta = "\"_meta\":{\"io.modelcontextprotocol/protocolVersion\":\"2026-07-28\"}";
        var d = J(await s.HandleAsync("{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"server/discover\",\"params\":{" + meta + "}}", default)).GetProperty("result");
        Assert.Equal("complete", d.GetProperty("resultType").GetString());
        Assert.Contains("2026-07-28", d.GetProperty("supportedVersions").EnumerateArray().Select(v => v.GetString()));
        var call = J(await s.HandleAsync("{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/call\",\"params\":{\"name\":\"recall_status\"," + meta + "}}", default));
        Assert.Equal("complete", call.GetProperty("result").GetProperty("resultType").GetString());
        var e = J(await s.HandleAsync("{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"tools/list\",\"params\":{\"_meta\":{\"io.modelcontextprotocol/protocolVersion\":\"1900-01-01\"}}}", default));
        Assert.Equal(-32022, e.GetProperty("error").GetProperty("code").GetInt32());
        Assert.Equal("1900-01-01", e.GetProperty("error").GetProperty("data").GetProperty("requested").GetString());
    }
}
