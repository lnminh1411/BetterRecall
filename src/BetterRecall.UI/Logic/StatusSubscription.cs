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
    public event Action<AgentErrorKind>? Disconnected;

    public TimeSpan MinBackoff { get; init; } = TimeSpan.FromSeconds(1);
    public TimeSpan MaxBackoff { get; init; } = TimeSpan.FromSeconds(30);

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
                            while (await conn.ReadLineAsync(ct).ConfigureAwait(false) is { } line)
                            {
                                var r = AgentClient.Parse(line);
                                if (!r.Ok) continue;
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
