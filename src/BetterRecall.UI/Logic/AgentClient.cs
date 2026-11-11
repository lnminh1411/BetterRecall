// SPDX-License-Identifier: MIT
// Copyright (c) BetterRecall contributors
using System.IO.Pipes;
using System.Security.Principal;
using System.Text;
using BetterRecall.Contracts;

namespace BetterRecall.UI.Logic;

public interface IAgentClient
{
    /// <summary>Sends one request line, returns the classified response. Never throws except on cancellation.</summary>
    Task<AgentResult> SendAsync(string line, CancellationToken ct = default);

    /// <summary>Process id of the trusted server, 0 when not connected.</summary>
    int ServerPid { get; }
}

/// <summary>One trusted connection to a line-protocol pipe.</summary>
public sealed class AgentConnection : IAsyncDisposable
{
    public const int MaxResponseBytes = 16 * 1024 * 1024;

    private readonly NamedPipeClientStream _pipe;
    private readonly LineReader _reader;

    public int ServerPid { get; }

    private AgentConnection(NamedPipeClientStream pipe, int pid)
    {
        _pipe = pipe;
        _reader = new LineReader(pipe);
        ServerPid = pid;
    }

    /// <summary>Connects with Anonymous impersonation and a 1 s timeout, then runs the trust check.</summary>
    public static async Task<(AgentConnection? Connection, AgentErrorKind Error)> ConnectAsync(
        string pipeName, IServerTrust trust, TimeSpan timeout, CancellationToken ct)
    {
        var pipe = new NamedPipeClientStream(".", pipeName, PipeDirection.InOut, PipeOptions.Asynchronous, TokenImpersonationLevel.Anonymous);
        try
        {
            await pipe.ConnectAsync(timeout, ct).ConfigureAwait(false);
        }
        catch (OperationCanceledException) when (ct.IsCancellationRequested) { await pipe.DisposeAsync(); throw; }
        catch (Exception ex) when (ex is TimeoutException or IOException or UnauthorizedAccessException or OperationCanceledException)
        {
            await pipe.DisposeAsync();
            return (null, AgentErrorKind.NotRunning);
        }

        TrustCheck check;
        try { check = trust.Verify(pipe.SafePipeHandle); }
        catch { check = new TrustCheck(false, 0); }
        if (!check.Trusted)
        {
            await pipe.DisposeAsync();
            return (null, AgentErrorKind.NotTrusted);
        }
        return (new AgentConnection(pipe, check.ServerPid), AgentErrorKind.None);
    }

    public async Task WriteLineAsync(string line, CancellationToken ct)
    {
        var bytes = Encoding.UTF8.GetBytes(line + "\n");
        try
        {
            await _pipe.WriteAsync(bytes, ct).ConfigureAwait(false);
            await _pipe.FlushAsync(ct).ConfigureAwait(false);
        }
        finally
        {
            // Request lines can carry a passphrase: do not leave the encoded copy around.
            Array.Clear(bytes);
        }
    }

    public Task<string?> ReadLineAsync(CancellationToken ct) => _reader.ReadLineAsync(MaxResponseBytes, ct);

    public ValueTask DisposeAsync() => _pipe.DisposeAsync();
}

/// <summary>Request/response client. Serialises requests on one connection, reconnects on demand.</summary>
public sealed class AgentClient(string pipeName, IServerTrust trust, TimeSpan? connectTimeout = null) : IAgentClient, IAsyncDisposable
{
    private readonly SemaphoreSlim _gate = new(1, 1);
    private readonly TimeSpan _timeout = connectTimeout ?? TimeSpan.FromSeconds(1);
    private AgentConnection? _conn;

    public string PipeName => pipeName;
    public int ServerPid => _conn?.ServerPid ?? 0;

    /// <summary>Raised after every response, with its classification. Used for the locked screen and the offline banner.</summary>
    public event Action<AgentErrorKind>? ResponseClassified;

    public async Task<AgentResult> SendAsync(string line, CancellationToken ct = default)
    {
        if (Encoding.UTF8.GetByteCount(line) > LineProtocol.MaxRequestBytes)
            return AgentResult.Fail(AgentErrorKind.Server, LineProtocol.TooLongError);

        await _gate.WaitAsync(ct).ConfigureAwait(false);
        AgentResult result;
        try
        {
            result = await SendLockedAsync(line, ct).ConfigureAwait(false);
        }
        finally { _gate.Release(); }
        ResponseClassified?.Invoke(result.Kind);
        return result;
    }

    /// <summary>
    /// A request with a multi-line reply (chat). Each line is passed to onLine; onLine returns true for the line that
    /// ends the reply. An error envelope always ends it. Returns the last line's result. Cancelling drops the connection,
    /// which also cancels the request on the agent.
    /// </summary>
    public async Task<AgentResult> StreamAsync(string line, Func<AgentResult, bool> onLine, CancellationToken ct = default)
    {
        if (Encoding.UTF8.GetByteCount(line) > LineProtocol.MaxRequestBytes)
            return AgentResult.Fail(AgentErrorKind.Server, LineProtocol.TooLongError);
        await _gate.WaitAsync(ct).ConfigureAwait(false);
        AgentResult result;
        try
        {
            if (_conn is null)
            {
                var (c, err) = await AgentConnection.ConnectAsync(pipeName, trust, _timeout, ct).ConfigureAwait(false);
                if (c is null) { result = AgentResult.Fail(err); goto done; }
                _conn = c;
            }
            try
            {
                await _conn.WriteLineAsync(line, ct).ConfigureAwait(false);
                while (true)
                {
                    var response = await _conn.ReadLineAsync(ct).ConfigureAwait(false);
                    if (response is null) { await DropAsync().ConfigureAwait(false); result = AgentResult.Fail(AgentErrorKind.NotRunning); break; }
                    result = Parse(response);
                    if (!result.Ok || onLine(result)) break;
                }
            }
            catch (OperationCanceledException)
            {
                await DropAsync().ConfigureAwait(false);
                throw;
            }
            catch (Exception ex) when (ex is IOException or ObjectDisposedException or InvalidOperationException)
            {
                await DropAsync().ConfigureAwait(false);
                result = AgentResult.Fail(AgentErrorKind.NotRunning);
            }
        done:;
        }
        finally { _gate.Release(); }
        ResponseClassified?.Invoke(result.Kind);
        return result;
    }

    private async Task<AgentResult> SendLockedAsync(string line, CancellationToken ct)
    {
        if (_conn is null)
        {
            var (c, err) = await AgentConnection.ConnectAsync(pipeName, trust, _timeout, ct).ConfigureAwait(false);
            if (c is null) return AgentResult.Fail(err);
            _conn = c;
        }

        string? response;
        try
        {
            await _conn.WriteLineAsync(line, ct).ConfigureAwait(false);
            response = await _conn.ReadLineAsync(ct).ConfigureAwait(false);
        }
        catch (OperationCanceledException)
        {
            // The response may still arrive later and would desync the stream: drop the connection.
            await DropAsync().ConfigureAwait(false);
            throw;
        }
        catch (Exception ex) when (ex is IOException or ObjectDisposedException or InvalidOperationException)
        {
            await DropAsync().ConfigureAwait(false);
            return AgentResult.Fail(AgentErrorKind.NotRunning);
        }

        if (response is null)
        {
            await DropAsync().ConfigureAwait(false);
            return AgentResult.Fail(AgentErrorKind.NotRunning);
        }
        return Parse(response);
    }

    public static AgentResult Parse(string response)
    {
        try
        {
            var (ok, data, error) = Envelope.Parse(response);
            return AgentResult.FromEnvelope(ok, data, error);
        }
        catch (System.Text.Json.JsonException)
        {
            return AgentResult.Fail(AgentErrorKind.Protocol, "Bad response.");
        }
    }

    private async Task DropAsync()
    {
        var c = _conn;
        _conn = null;
        if (c is not null) await c.DisposeAsync().ConfigureAwait(false);
    }

    public async ValueTask DisposeAsync()
    {
        await _gate.WaitAsync().ConfigureAwait(false);
        try { await DropAsync().ConfigureAwait(false); }
        finally { _gate.Release(); }
    }
}
