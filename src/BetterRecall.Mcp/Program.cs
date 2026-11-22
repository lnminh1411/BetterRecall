// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Text;
using BetterRecall.Contracts;
using BetterRecall.Mcp;

// betterrecall-mcp: MCP server over stdio. Holds no key and opens no database: every tool call is one request
// on the MCP pipe of the running agent, after the client trust check (research 7.4, 7.8). stdout carries only JSON-RPC.

IntegrityCheck.EnforceOrExit(AppContext.BaseDirectory, gui: false);

var pipeName = PipeNames.Mcp(PipeNames.UserSid(), DataPaths.Home());   // the agent of this data folder only
var agentExe = InstallLayout.Full(AppContext.BaseDirectory, InstallLayout.AgentExe);
var relayed = Environment.GetEnvironmentVariable(InstallLayout.McpRelayEnv) == "1";
var pipe = new McpPipe(pipeName, agentExe, AppContext.BaseDirectory, relayed);

var server = new McpServer(pipe.CallAsync);
var utf8 = new UTF8Encoding(false);
using var input = new StreamReader(Console.OpenStandardInput(), utf8);
using var output = new StreamWriter(Console.OpenStandardOutput(), utf8) { AutoFlush = true, NewLine = "\n" };
return await McpHost.RunAsync(input, output, server, pipe);
