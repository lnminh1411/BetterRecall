// SPDX-License-Identifier: MIT
// Copyright (c) BetterRecall contributors
using System.IO.Pipes;
using System.Security.AccessControl;
using System.Security.Principal;
using BetterRecall.Contracts;

namespace BetterRecall.UI.Logic.Mock;

/// <summary>
/// Hosts a MockAgent on a real named pipe so the UI exercises its normal client path (connect, trust check, framing).
/// The pipe grants access to the current user only.
/// </summary>
public sealed class MockPipeServer(MockAgent agent, string pipeName) : IAsyncDisposable
{
    private readonly CancellationTokenSource _cts = new();
    private readonly List<Task> _sessions = [];
    private Task? _accept;

    public string PipeName => pipeName;

    /// <summary>Status lines pushed to subscribers at least this often.</summary>
    public TimeSpan SubscribeInterval { get; init; } = TimeSpan.FromSeconds(10);

    public static string NameFor(int pid) => $"BetterRecall.ui.mock.{PipeNames.UserSid()}.{pid}";

    public void Start() => _accept ??= Task.Run(AcceptLoopAsync);

    private NamedPipeServerStream Create()
    {
        var sec = new PipeSecurity();
        var user = WindowsIdentity.GetCurrent().User!;
        sec.AddAccessRule(new PipeAccessRule(user, PipeAccessRights.FullControl, AccessControlType.Allow));
        return NamedPipeServerStreamAcl.Create(pipeName, PipeDirection.InOut, NamedPipeServerStream.MaxAllowedServerInstances,
            PipeTransmissionMode.Byte, PipeOptions.Asynchronous, 0, 0, sec);
    }

    private async Task AcceptLoopAsync()
    {
        var ct = _cts.Token;
        while (!ct.IsCancellationRequested)
        {
            NamedPipeServerStream server;
            try
            {
                server = Create();
                await server.WaitForConnectionAsync(ct).ConfigureAwait(false);
            }
            catch (OperationCanceledException) { return; }
            catch (IOException) { await Task.Delay(50, CancellationToken.None).ConfigureAwait(false); continue; }
            lock (_sessions)
            {
                _sessions.RemoveAll(t => t.IsCompleted);
                _sessions.Add(Task.Run(() => SessionAsync(server, ct)));
            }
        }
    }

    private async Task SessionAsync(NamedPipeServerStream pipe, CancellationToken ct)
    {
        await using var _ = pipe;
        try
        {
            while (!ct.IsCancellationRequested)
            {
                string? line;
                try { line = await LineProtocol.ReadLineAsync(pipe, LineProtocol.MaxRequestBytes, ct).ConfigureAwait(false); }
                catch (LineTooLongException)
                {
                    await LineProtocol.WriteLineAsync(pipe, Envelope.Error(LineProtocol.TooLongError), ct).ConfigureAwait(false);
                    return;
                }
                if (line is null) return;
                var response = await agent.HandleAsync(line, ct).ConfigureAwait(false);
                await LineProtocol.WriteLineAsync(pipe, response, ct).ConfigureAwait(false);
                if (line.Trim().Equals("subscribe", StringComparison.OrdinalIgnoreCase) && response.StartsWith("{\"ok\":true", StringComparison.Ordinal))
                {
                    await StreamStatusAsync(pipe, ct).ConfigureAwait(false);
                    return;
                }
            }
        }
        catch (Exception ex) when (ex is IOException or OperationCanceledException or ObjectDisposedException) { }
    }

    private async Task StreamStatusAsync(NamedPipeServerStream pipe, CancellationToken ct)
    {
        var signal = new SemaphoreSlim(0);
        void OnChange() => signal.Release();
        agent.StatusChanged += OnChange;
        try
        {
            while (!ct.IsCancellationRequested && pipe.IsConnected)
            {
                var json = System.Text.Json.JsonSerializer.Serialize(agent.Status(), ContractsJson.Default.AgentStatus);
                await LineProtocol.WriteLineAsync(pipe, Envelope.Ok(json), ct).ConfigureAwait(false);
                await signal.WaitAsync(SubscribeInterval, ct).ConfigureAwait(false);
                while (signal.CurrentCount > 0) signal.Wait(0);
            }
        }
        finally { agent.StatusChanged -= OnChange; }
    }

    public async ValueTask DisposeAsync()
    {
        _cts.Cancel();
        try { if (_accept is not null) await _accept.ConfigureAwait(false); } catch { }
        Task[] sessions;
        lock (_sessions) sessions = [.. _sessions];
        try { await Task.WhenAll(sessions).WaitAsync(TimeSpan.FromSeconds(2)).ConfigureAwait(false); } catch { }
        _cts.Dispose();
    }
}
