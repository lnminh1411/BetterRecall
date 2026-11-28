// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.IO.Pipes;
using System.Text.Json;
using System.Text.Json.Nodes;
using System.Xml.Linq;
using BetterRecall.Contracts;
using BetterRecall.UI.Logic;
using BetterRecall.UI.Logic.Mock;
using Microsoft.Win32.SafeHandles;

namespace BetterRecall.UI.Tests;

/// <summary>Line server whose answers come from a function. Null closes the connection; Restart drops every live pipe.</summary>
public sealed class FakeLineServer : IAsyncDisposable
{
    private readonly CancellationTokenSource _cts = new();
    private readonly List<NamedPipeServerStream> _live = [];
    private readonly Task _loop;
    private int _connections;

    public string Name { get; } = "BetterRecall.uitest." + Guid.NewGuid().ToString("N");
    public int Connections => Volatile.Read(ref _connections);
    public List<string> Received { get; } = [];

    public FakeLineServer(Func<string, string?> answer)
        : this(line => answer(line) is { } a ? [a] : null) { }

    public FakeLineServer(Func<string, IEnumerable<string>?> answerMany)
    {
        _loop = Task.Run(async () =>
        {
            while (!_cts.IsCancellationRequested)
            {
                var server = new NamedPipeServerStream(Name, PipeDirection.InOut, NamedPipeServerStream.MaxAllowedServerInstances,
                    PipeTransmissionMode.Byte, PipeOptions.Asynchronous);
                try { await server.WaitForConnectionAsync(_cts.Token); }
                catch (OperationCanceledException) { await server.DisposeAsync(); return; }
                Interlocked.Increment(ref _connections);
                lock (_live) _live.Add(server);
                _ = Task.Run(async () =>
                {
                    await using var s = server;
                    try
                    {
                        while (await LineProtocol.ReadLineAsync(s, LineProtocol.MaxUiRequestBytes, _cts.Token) is { } line)
                        {
                            lock (Received) Received.Add(line);
                            var replies = answerMany(line);
                            if (replies is null) return;
                            foreach (var a in replies)
                                await LineProtocol.WriteLineAsync(s, a, _cts.Token);
                        }
                    }
                    catch { }
                    finally { lock (_live) _live.Remove(s); }
                });
            }
        });
    }

    /// <summary>Closes every live connection, simulating an agent restart.</summary>
    public void Restart()
    {
        lock (_live)
        {
            foreach (var s in _live) s.Dispose();
            _live.Clear();
        }
    }

    public async ValueTask DisposeAsync()
    {
        _cts.Cancel();
        Restart();
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

/// <summary>Trust verifier that accepts any server pipe in tests where trust is not under test.</summary>
public sealed class AcceptAllTrust : IServerTrust
{
    public TrustCheck Verify(SafePipeHandle pipe) => new(true, Environment.ProcessId);
}

/// <summary>In-memory fake IAgentClient that records lines and delegates or returns scripted replies.</summary>
public sealed class ScriptedClient(Func<string, CancellationToken, Task<AgentResult>> answer, Predicate<string>? recordFilter = null) : IAgentClient
{
    public ScriptedClient(Func<string, Task<AgentResult>> answer) : this((line, _) => answer(line)) { }

    public List<string> Lines { get; } = [];
    public int ServerPid { get; init; } = 1;

    public static ScriptedClient FixedOk(string dataJson) =>
        new((string _, CancellationToken _) => Task.FromResult(AgentClient.Parse(Envelope.Ok(dataJson))));

    public static ScriptedClient Recording(IAgentClient inner, Predicate<string>? filter = null) =>
        new((line, ct) => inner.SendAsync(line, ct), filter) { ServerPid = inner.ServerPid };

    public Task<AgentResult> SendAsync(string line, CancellationToken ct = default)
    {
        if (recordFilter is null || recordFilter(line))
            lock (Lines) Lines.Add(line);
        return answer(line, ct);
    }
}

/// <summary>Holds every request until the gate opens, then forwards it: an answer that arrives "late".</summary>
public sealed class GateClient(IAgentClient inner) : IAgentClient
{
    public TaskCompletionSource Gate { get; } = new(TaskCreationOptions.RunContinuationsAsynchronously);
    public int ServerPid => inner.ServerPid;

    public async Task<AgentResult> SendAsync(string line, CancellationToken ct = default)
    {
        await Gate.Task;
        return await inner.SendAsync(line, ct);
    }
}

/// <summary>Shared repo path and Resources.resw loaders for UI tests.</summary>
public static class TestResources
{
    public static string RepoRoot()
    {
        for (var dir = new DirectoryInfo(AppContext.BaseDirectory); dir is not null; dir = dir.Parent)
            if (File.Exists(Path.Combine(dir.FullName, "BetterRecall.slnx"))) return dir.FullName;
        throw new DirectoryNotFoundException("Repo root (BetterRecall.slnx) not found from " + AppContext.BaseDirectory);
    }

    public static string UiDir() => Path.Combine(RepoRoot(), "src", "BetterRecall.UI");

    public static Dictionary<string, string> Strings()
    {
        var resw = XDocument.Load(Path.Combine(UiDir(), "Strings", "en-US", "Resources.resw"));
        return resw.Root!.Elements("data").ToDictionary(e => (string)e.Attribute("name")!, e => (string)e.Element("value")!);
    }

    public static JsonObject DefaultSettings() =>
        (JsonObject)JsonNode.Parse(JsonSerializer.Serialize(new Settings(), ContractsJson.Default.Settings))!;
}
