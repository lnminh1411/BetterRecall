// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

namespace BetterRecall.UI.Logic;

/// <summary>
/// Runs one long request (export, replay export) on a connection of its own. The shared client stays free for the
/// timeline, settings and detail views meanwhile, and cancelling drops only this connection, which also cancels the
/// request on the agent (export and replay export are long-running verbs there).
/// </summary>
public static class OwnConnection
{
    public static async Task<T> RunAsync<T>(Func<IAgentClient> newClient, Func<AgentApi, CancellationToken, Task<T>> call, CancellationToken ct)
    {
        var client = newClient();
        try { return await call(new AgentApi(client), ct).ConfigureAwait(false); }
        finally
        {
            if (client is IAsyncDisposable d) await d.DisposeAsync().ConfigureAwait(false);
        }
    }
}
