// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using BetterRecall.Contracts;
using BetterRecall.Core.Ipc;

namespace BetterRecall.Mcp;

/// <summary>
/// One tool call = one request on the agent's MCP pipe, after the client trust check (the agent must be the exe next to
/// this server). B5/C4: after a launcher upgrade the agent runs from the new install folder, so a server started before
/// it fails that check for good, and its exe keeps the old folder from being deleted. Such a server is stale: under
/// the bin\betterrecall-mcp.exe relay it exits with McpRestartExitCode without answering, and the relay restarts it
/// from the current install and sends the call again; started any other way it says to restart the MCP client.
/// </summary>
public sealed class McpPipe(string pipeName, string agentExe, string installDir, bool relayed)
{
    public const string NotTrusted = "BetterRecall not trusted.";
    public const string Updated = "BetterRecall was updated while this MCP session ran: restart your MCP client (or its BetterRecall server) to reconnect.";

    /// <summary>Set once a call found this server stale; the host loop then exits.</summary>
    public bool Stale { get; private set; }

    public bool Relayed => relayed;

    public async Task<PipeAnswer> CallAsync(string line, CancellationToken ct)
    {
        var (result, client) = await PipeClient.ConnectAsync(pipeName, agentExe, 1000, ct).ConfigureAwait(false);
        if (result == PipeConnectResult.NotRunning)
            return new PipeAnswer(false, "BetterRecall not running, or MCP is off (setting mcp.enabled).");
        if (result != PipeConnectResult.Ok || client == null)
        {
            if (!IsStale(installDir)) return new PipeAnswer(false, NotTrusted);
            Stale = true;
            return new PipeAnswer(false, Updated);
        }
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

    /// <summary>True when installDir is a launcher install (&lt;root&gt;\&lt;version&gt;-&lt;hash12&gt;) and root's bin\current
    /// names another one: the launcher has moved on, and the agent this server trusts is gone.</summary>
    public static bool IsStale(string installDir)
    {
        var dir = Path.GetFullPath(installDir).TrimEnd(Path.DirectorySeparatorChar);
        var name = Path.GetFileName(dir);
        if (!InstallLayout.IsVersionFolder(name) || Path.GetDirectoryName(dir) is not { } root) return false;
        var current = InstallLayout.CurrentInstall(root);
        return current is not null && !string.Equals(Path.GetFileName(current), name, StringComparison.OrdinalIgnoreCase)
            && File.Exists(Path.Combine(current, InstallLayout.AgentExe));
    }
}

/// <summary>The stdio loop: one JSON-RPC message per line in, one reply line out (none for notifications).</summary>
public static class McpHost
{
    /// <summary>Returns the process exit code: 0 at end of input, McpRestartExitCode once the server is stale.</summary>
    public static async Task<int> RunAsync(TextReader input, TextWriter output, McpServer server, McpPipe pipe)
    {
        while (await input.ReadLineAsync().ConfigureAwait(false) is { } line)
        {
            if (line.Length == 0) continue;
            if (line[0] == '﻿') line = line[1..];
            string? reply;
            try { reply = await server.HandleAsync(line, CancellationToken.None).ConfigureAwait(false); }
            catch (Exception ex)
            {
                Console.Error.WriteLine("betterrecall-mcp: " + ex.GetType().Name);
                reply = null;
            }
            // Under the relay the call stays unanswered: the relay sends it to the server of the current install.
            if (pipe.Stale && pipe.Relayed) return InstallLayout.McpRestartExitCode;
            if (reply != null) await output.WriteLineAsync(reply).ConfigureAwait(false);
            if (pipe.Stale)
            {
                await output.FlushAsync().ConfigureAwait(false);
                return InstallLayout.McpRestartExitCode;   // frees the old install folder
            }
        }
        return 0;
    }
}
