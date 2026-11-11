// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Text;
using BetterRecall.Contracts;
using BetterRecall.Core.Ipc;
using BetterRecall.Mcp;

// betterrecall-mcp: MCP server over stdio. Holds no key and opens no database: every tool call is one request
// on the MCP pipe of the running agent, after the client trust check (research 7.4, 7.8). stdout carries only JSON-RPC.

var pipeName = PipeNames.Mcp(PipeNames.UserSid());
var agentExe = InstallLayout.Full(AppContext.BaseDirectory, InstallLayout.AgentExe);

async Task<PipeAnswer> CallPipe(string line, CancellationToken ct)
{
    var (result, client) = await PipeClient.ConnectAsync(pipeName, agentExe, 1000, ct).ConfigureAwait(false);
    if (result == PipeConnectResult.NotRunning)
        return new PipeAnswer(false, "BetterRecall not running, or MCP is off (setting mcp.enabled).");
    if (result != PipeConnectResult.Ok || client == null) return new PipeAnswer(false, "BetterRecall not trusted.");
    await using (client)
    {
        string? response;
        try { response = await client.RequestAsync(line, ct).ConfigureAwait(false); }
        catch (IOException) { response = null; }
        if (response == null) return new PipeAnswer(false, "No response from BetterRecall.");
        var (ok, data, error) = Envelope.Parse(response);
        return ok ? new PipeAnswer(true, data.ValueKind == System.Text.Json.JsonValueKind.Undefined ? "null" : data.GetRawText())
                  : new PipeAnswer(false, error ?? "Internal error.");
    }
}

var server = new McpServer(CallPipe);
var utf8 = new UTF8Encoding(false);
using var input = new StreamReader(Console.OpenStandardInput(), utf8);
using var output = new StreamWriter(Console.OpenStandardOutput(), utf8) { AutoFlush = true, NewLine = "\n" };

while (await input.ReadLineAsync() is { } line)
{
    if (line.Length == 0) continue;
    if (line[0] == '﻿') line = line[1..];
    string? reply;
    try { reply = await server.HandleAsync(line, CancellationToken.None); }
    catch (Exception ex)
    {
        Console.Error.WriteLine("betterrecall-mcp: " + ex.GetType().Name);
        reply = null;
    }
    if (reply != null) await output.WriteLineAsync(reply);
}
return 0;
