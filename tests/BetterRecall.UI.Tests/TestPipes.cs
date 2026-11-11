// SPDX-License-Identifier: MIT
// Copyright (c) BetterRecall contributors
using System.IO.Pipes;
using BetterRecall.Contracts;
using BetterRecall.UI.Logic;
using BetterRecall.UI.Logic.Mock;

namespace BetterRecall.UI.Tests;

/// <summary>Line server whose answers come from a function. Null closes the connection.</summary>
public sealed class FakeLineServer : IAsyncDisposable
{
    private readonly CancellationTokenSource _cts = new();
    private readonly Task _loop;
    public string Name { get; } = "BetterRecall.uitest." + Guid.NewGuid().ToString("N");
    public List<string> Received { get; } = [];

    public FakeLineServer(Func<string, string?> answer)
    {
        _loop = Task.Run(async () =>
        {
            while (!_cts.IsCancellationRequested)
            {
                var server = new NamedPipeServerStream(Name, PipeDirection.InOut, NamedPipeServerStream.MaxAllowedServerInstances,
                    PipeTransmissionMode.Byte, PipeOptions.Asynchronous);
                try { await server.WaitForConnectionAsync(_cts.Token); }
                catch (OperationCanceledException) { await server.DisposeAsync(); return; }
                _ = Task.Run(async () =>
                {
                    await using var s = server;
                    try
                    {
                        while (await LineProtocol.ReadLineAsync(s, 4096, _cts.Token) is { } line)
                        {
                            lock (Received) Received.Add(line);
                            var a = answer(line);
                            if (a is null) return;
                            await LineProtocol.WriteLineAsync(s, a, _cts.Token);
                        }
                    }
                    catch { }
                });
            }
        });
    }

    public async ValueTask DisposeAsync()
    {
        _cts.Cancel();
        try { await _loop; } catch { }
    }
}

/// <summary>MockAgent on a real pipe plus a trusted client (the test process is the expected server image).</summary>
public sealed class MockHarness : IAsyncDisposable
{
    public MockAgent Agent { get; }
    public MockPipeServer Server { get; }
    public AgentClient Client { get; }
    public AgentApi Api { get; }

    public MockHarness(MockOptions? options = null)
    {
        Agent = new MockAgent(options) { SlowVerbDelay = TimeSpan.Zero };
        Server = new MockPipeServer(Agent, "BetterRecall.uitest.mock." + Guid.NewGuid().ToString("N")) { SubscribeInterval = TimeSpan.FromMilliseconds(200) };
        Server.Start();
        Client = new AgentClient(Server.PipeName, OwnProcessTrust());
        Api = new AgentApi(Client);
    }

    public static ServerTrustVerifier OwnProcessTrust() => new(Environment.ProcessPath!);

    public async ValueTask DisposeAsync()
    {
        await Client.DisposeAsync();
        await Server.DisposeAsync();
    }
}
