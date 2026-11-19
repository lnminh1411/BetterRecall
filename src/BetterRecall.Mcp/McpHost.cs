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
public sealed class McpPipe(string pipeName, string agentExe, string installDir, bool relayed, TimeSpan? staleGrace = null)
{
    public const string NotTrusted = "BetterRecall not trusted.";
    public const string Updated = "BetterRecall was updated while this MCP session ran: restart your MCP client (or its BetterRecall server) to reconnect.";
    private const string LauncherMutex = @"Local\BetterRecall.Launcher";

    private readonly TimeSpan _staleGrace = staleGrace ?? TimeSpan.FromSeconds(2);

    /// <summary>Set once a call found this server stale; the host loop then exits.</summary>
    public bool Stale { get; private set; }

    public bool Relayed => relayed;

    public async Task<PipeAnswer> CallAsync(string line, CancellationToken ct)
    {
        if (IsStale(installDir))
        {
            Stale = true;
            return new PipeAnswer(false, Updated);
        }
        var (result, client) = await ConnectWithUpgradeGraceAsync(ct).ConfigureAwait(false);
        if (result == PipeConnectResult.NotRunning)
        {
            if (await WaitForStaleAsync(allowWait: IsLauncherActive(), ct).ConfigureAwait(false))
            {
                Stale = true;
                return new PipeAnswer(false, Updated);
            }
            return new PipeAnswer(false, "BetterRecall not running, or MCP is off (setting mcp.enabled).");
        }
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
            if (response == null)
            {
                if (await WaitForStaleAsync(allowWait: true, ct).ConfigureAwait(false))
                {
                    Stale = true;
                    return new PipeAnswer(false, Updated);
                }
                return new PipeAnswer(false, "No response from BetterRecall.");
            }
            var (ok, data, error) = Envelope.Parse(response);
            return ok ? new PipeAnswer(true, data.ValueKind == System.Text.Json.JsonValueKind.Undefined ? "null" : data.GetRawText())
                      : new PipeAnswer(false, error ?? "Internal error.");
        }
    }

    private async Task<(PipeConnectResult Result, PipeClient? Client)> ConnectWithUpgradeGraceAsync(CancellationToken ct)
    {
        var first = await PipeClient.ConnectAsync(pipeName, agentExe, 1000, ct).ConfigureAwait(false);
        if (first.Result != PipeConnectResult.NotRunning || !IsLauncherActive() || IsStale(installDir)) return first;
        for (int i = 0; i < 2; i++)
        {
            await Task.Delay(250, ct).ConfigureAwait(false);
            if (IsStale(installDir)) return first;
            var next = await PipeClient.ConnectAsync(pipeName, agentExe, 1000, ct).ConfigureAwait(false);
            if (next.Result != PipeConnectResult.NotRunning || !IsLauncherActive() || IsStale(installDir)) return next;
        }
        return first;
    }

    private async Task<bool> WaitForStaleAsync(bool allowWait, CancellationToken ct)
    {
        if (IsStale(installDir)) return true;
        if (!allowWait || !CanBeStale(installDir)) return false;
        var deadline = DateTime.UtcNow + (IsLauncherActive() && _staleGrace < TimeSpan.FromSeconds(6) ? TimeSpan.FromSeconds(6) : _staleGrace);
        while (DateTime.UtcNow < deadline)
        {
            await Task.Delay(50, ct).ConfigureAwait(false);
            if (IsStale(installDir)) return true;
        }
        return IsStale(installDir);
    }

    private static bool CanBeStale(string installDir)
    {
        var dir = Path.GetFullPath(installDir).TrimEnd(Path.DirectorySeparatorChar);
        return IsVersionFolderName(dir) && Path.GetDirectoryName(dir) is not null;
    }

    private static bool IsVersionFolderName(string dir) => InstallLayout.IsVersionFolder(Path.GetFileName(dir));

    private static bool IsLauncherActive()
    {
        try
        {
            if (!Mutex.TryOpenExisting(LauncherMutex, out var m)) return false;
            m.Dispose();
            return true;
        }
        catch (UnauthorizedAccessException) { return true; }
    }

    /// <summary>True when installDir is a launcher install (&lt;root&gt;\&lt;version&gt;-&lt;hash12&gt;) and root's bin\current
    /// names another one: the launcher has moved on, and the agent this server trusts is gone.</summary>
    public static bool IsStale(string installDir)
    {
        var dir = InstallLayout.RealPath(installDir).TrimEnd(Path.DirectorySeparatorChar);
        var name = Path.GetFileName(dir);
        if (!InstallLayout.IsVersionFolder(name) || Path.GetDirectoryName(dir) is not { } root) return false;
        var current = InstallLayout.CurrentInstall(root, InstallLayout.RealPath(root));
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
                reply = InternalError(line);   // S5: a request is always answered (the relay would hold it forever)
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

    /// <summary>JSON-RPC "Internal error" for the request in line (string or number id), or null for anything else.</summary>
    public static string? InternalError(string line)
    {
        try
        {
            using var doc = System.Text.Json.JsonDocument.Parse(line);
            var root = doc.RootElement;
            if (root.ValueKind != System.Text.Json.JsonValueKind.Object || !root.TryGetProperty("id", out var id)
                || id.ValueKind is not (System.Text.Json.JsonValueKind.String or System.Text.Json.JsonValueKind.Number)) return null;
            var buf = new System.Buffers.ArrayBufferWriter<byte>();
            using (var w = new System.Text.Json.Utf8JsonWriter(buf))
            {
                w.WriteStartObject();
                w.WriteString("jsonrpc", "2.0");
                w.WritePropertyName("id");
                id.WriteTo(w);
                w.WriteStartObject("error");
                w.WriteNumber("code", -32603);
                w.WriteString("message", "Internal error");
                w.WriteEndObject();
                w.WriteEndObject();
            }
            return System.Text.Encoding.UTF8.GetString(buf.WrittenSpan);
        }
        catch (System.Text.Json.JsonException) { return null; }
    }
}
