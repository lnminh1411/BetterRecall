// SPDX-License-Identifier: MIT
// Copyright (c) BetterRecall contributors
using System.Text.Json;
using BetterRecall.Contracts;

namespace BetterRecall.UI.Logic;

/// <summary>Second connection that sends "subscribe" and streams status lines (research.md 7.7). Reconnects with backoff.</summary>
public sealed class StatusSubscription(string pipeName, IServerTrust trust) : IAsyncDisposable
{
    private CancellationTokenSource? _cts;
    private Task? _loop;

    public event Action<AgentStatus>? StatusReceived;
    /// <summary>Captures left the store (B10).</summary>
    public event Action<RemovedEvent>? Removed;
    public event Action<AgentErrorKind>? Disconnected;
    /// <summary>The subscription was acknowledged: the first connection and every reconnect (agent restart, D2).</summary>
    public event Action? Connected;

    public TimeSpan MinBackoff { get; init; } = TimeSpan.FromSeconds(1);
    /// <summary>5 s: after an agent restart the UI is back within about 6 s (each try also waits up to 1 s to connect);
    /// at 30 s it took 20 to 45 s (sweep 3 D10). A try on a missing pipe costs next to nothing.</summary>
    public TimeSpan MaxBackoff { get; init; } = TimeSpan.FromSeconds(5);

    public void Start()
    {
        if (_loop is not null) return;
        _cts = new CancellationTokenSource();
        _loop = Task.Run(() => RunAsync(_cts.Token));
    }

    private async Task RunAsync(CancellationToken ct)
    {
        var backoff = MinBackoff;
        while (!ct.IsCancellationRequested)
        {
            AgentErrorKind reason = AgentErrorKind.NotRunning;
            try
            {
                var (conn, err) = await AgentConnection.ConnectAsync(pipeName, trust, TimeSpan.FromSeconds(1), ct).ConfigureAwait(false);
                if (conn is null) reason = err;
                else
                {
                    await using (conn)
                    {
                        await conn.WriteLineAsync("subscribe", ct).ConfigureAwait(false);
                        var ack = await conn.ReadLineAsync(ct).ConfigureAwait(false);
                        if (ack is not null && AgentClient.Parse(ack).Ok)
                        {
                            backoff = MinBackoff;
                            Connected?.Invoke();
                            while (await conn.ReadLineAsync(ct).ConfigureAwait(false) is { } line)
                            {
                                var r = AgentClient.Parse(line);
                                if (!r.Ok) continue;
                                if (Responses.Removed(r.Data) is { } removed) { Removed?.Invoke(removed); continue; }
                                var status = Responses.Status(r.Data);
                                if (status is not null) StatusReceived?.Invoke(status);
                            }
                        }
                    }
                }
            }
            catch (OperationCanceledException) when (ct.IsCancellationRequested) { return; }
            catch (Exception ex) when (ex is IOException or JsonException or ObjectDisposedException or InvalidOperationException) { }

            Disconnected?.Invoke(reason);
            try { await Task.Delay(backoff, ct).ConfigureAwait(false); }
            catch (OperationCanceledException) { return; }
            backoff = TimeSpan.FromTicks(Math.Min(MaxBackoff.Ticks, backoff.Ticks * 2));
        }
    }

    public async ValueTask DisposeAsync()
    {
        if (_cts is null) return;
        _cts.Cancel();
        try { if (_loop is not null) await _loop.ConfigureAwait(false); } catch { }
        _cts.Dispose();
        _cts = null;
        _loop = null;
    }
}
