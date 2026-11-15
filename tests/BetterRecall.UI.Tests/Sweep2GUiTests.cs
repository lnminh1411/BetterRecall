// SPDX-License-Identifier: MIT
// Copyright (c) BetterRecall contributors
using System.IO.Pipes;
using BetterRecall.Contracts;
using BetterRecall.UI.Logic;

namespace BetterRecall.UI.Tests;

/// <summary>Bug sweep 2, batch G: the first request after an agent restart (C20, D30).</summary>
public class Sweep2GUiTests
{
    /// <summary>Line server whose connections can be cut from the server side, like an agent that restarted.</summary>
    private sealed class RestartableServer : IAsyncDisposable
    {
        private readonly CancellationTokenSource _cts = new();
        private readonly List<NamedPipeServerStream> _live = [];
        private readonly Task _loop;
        public string Name { get; } = "BetterRecall.uitest.restart." + Guid.NewGuid().ToString("N");
        public int Connections;
        public List<string> Received { get; } = [];

        public RestartableServer()
        {
            _loop = Task.Run(async () =>
            {
                while (!_cts.IsCancellationRequested)
                {
                    var server = new NamedPipeServerStream(Name, PipeDirection.InOut, NamedPipeServerStream.MaxAllowedServerInstances,
                        PipeTransmissionMode.Byte, PipeOptions.Asynchronous);
                    try { await server.WaitForConnectionAsync(_cts.Token); }
                    catch (OperationCanceledException) { await server.DisposeAsync(); return; }
                    Interlocked.Increment(ref Connections);
                    lock (_live) _live.Add(server);
                    _ = Task.Run(async () =>
                    {
                        try
                        {
                            while (await LineProtocol.ReadLineAsync(server, 4096, _cts.Token) is { } line)
                            {
                                lock (Received) Received.Add(line);
                                await LineProtocol.WriteLineAsync(server, Envelope.Ok("\"" + line.Split(' ')[0] + "\""), _cts.Token);
                            }
                        }
                        catch { }
                    });
                }
            });
        }

        /// <summary>Closes every live connection (the agent process went away).</summary>
        public void Restart()
        {
            lock (_live) { foreach (var s in _live) s.Dispose(); _live.Clear(); }
        }

        public async ValueTask DisposeAsync()
        {
            _cts.Cancel();
            Restart();
            try { await _loop; } catch { }
        }
    }

    [Fact]
    public async Task First_request_after_an_agent_restart_is_sent_again_on_a_fresh_connection()
    {
        await using var server = new RestartableServer();
        await using var client = new AgentClient(server.Name, MockHarness.OwnProcessTrust());
        Assert.True((await client.SendAsync("status")).Ok);
        server.Restart();
        await Task.Delay(100);
        var r = await client.SendAsync("timeline 2026-11-15T00:00:00Z 2026-11-16T00:00:00Z");
        Assert.True(r.Ok, r.Error);
        Assert.Equal(2, server.Connections);

        // A verb that changes something is also resent when the stale write failed (the agent never saw it).
        server.Restart();
        await Task.Delay(100);
        Assert.True((await client.SendAsync("purge confirm abc")).Ok);
        Assert.Equal(3, server.Connections);
        lock (server.Received) Assert.Single(server.Received, l => l == "purge confirm abc");
    }

    [Theory]
    [InlineData("status", true)]
    [InlineData("search {\"q\":\"x\"}", true)]
    [InlineData("timeline a b", true)]
    [InlineData("chat get 12", true)]
    [InlineData("settings get", true)]
    [InlineData("history", true)]
    [InlineData("history clear", false)]
    [InlineData("settings set {}", false)]
    [InlineData("purge confirm t", false)]
    [InlineData("unlock secret", false)]
    [InlineData("panic", false)]
    [InlineData("chat delete 1", false)]
    public void Only_reading_verbs_are_resent_after_a_lost_reply(string line, bool readOnly)
        => Assert.Equal(readOnly, AgentClient.IsReadOnly(line));
}
