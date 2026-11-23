// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using BetterRecall.Contracts;
using BetterRecall.UI.Logic;

namespace BetterRecall.Agent.Conformance.Harness;

public enum PipeConnectResult { Ok, NotRunning, NotTrusted }

/// <summary>
/// Line client for the agent's public, UI and MCP pipes, built on the UI's own pipe client and server trust check
/// (linked from src\BetterRecall.UI\Logic), so the suite exercises the code that ships instead of a test-only client.
/// </summary>
public sealed class PipeClient : IAsyncDisposable
{
    private readonly AgentConnection _conn;

    private PipeClient(AgentConnection conn) => _conn = conn;

    public int ServerPid => _conn.ServerPid;

    public static async Task<(PipeConnectResult Result, PipeClient? Client)> ConnectAsync(
        string pipeName, string expectedServerImage, int timeoutMs = 1000, CancellationToken ct = default)
    {
        var (conn, error) = await AgentConnection.ConnectAsync(
            PipeNames.Base(pipeName), new ServerTrustVerifier(expectedServerImage), TimeSpan.FromMilliseconds(timeoutMs), ct)
            .ConfigureAwait(false);
        return error switch
        {
            AgentErrorKind.None when conn is not null => (PipeConnectResult.Ok, new PipeClient(conn)),
            AgentErrorKind.NotTrusted => (PipeConnectResult.NotTrusted, null),
            _ => (PipeConnectResult.NotRunning, null),
        };
    }

    /// <summary>Sends one request line and reads one response line. Null when the server closed the pipe.</summary>
    public async Task<string?> RequestAsync(string line, CancellationToken ct = default)
    {
        await _conn.WriteLineAsync(line.Replace("\r", " ").Replace("\n", " "), ct).ConfigureAwait(false);
        return await _conn.ReadLineAsync(ct).ConfigureAwait(false);
    }

    public Task<string?> ReadLineAsync(CancellationToken ct = default) => _conn.ReadLineAsync(ct);

    public ValueTask DisposeAsync() => _conn.DisposeAsync();
}
