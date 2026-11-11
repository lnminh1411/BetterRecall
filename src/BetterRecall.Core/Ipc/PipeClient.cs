// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.IO.Pipes;
using System.Security.Principal;
using BetterRecall.Contracts;
using BetterRecall.Core.Native;

namespace BetterRecall.Core.Ipc;

public enum PipeConnectResult { Ok, NotRunning, NotTrusted }

/// <summary>
/// Client side of the line protocol with the research 7.4 client checks: anonymous impersonation level,
/// 1 s connect timeout, server image path must equal the expected agent path, same session.
/// </summary>
public sealed class PipeClient : IAsyncDisposable, IDisposable
{
    public const int MaxResponseBytes = 16 * 1024 * 1024;

    private readonly NamedPipeClientStream _stream;
    private readonly LineReader _reader;

    private PipeClient(NamedPipeClientStream stream)
    {
        _stream = stream;
        _reader = new LineReader(stream);
    }

    public int ServerPid { get; private set; }

    /// <summary>Connects and runs the trust check. Client is null unless the result is Ok.</summary>
    public static async Task<(PipeConnectResult Result, PipeClient? Client)> ConnectAsync(
        string pipeName, string expectedServerImage, int timeoutMs = 1000, CancellationToken ct = default)
    {
        var stream = new NamedPipeClientStream(".", PipeNames.Base(pipeName), PipeDirection.InOut, PipeOptions.Asynchronous, TokenImpersonationLevel.Anonymous);
        try
        {
            await stream.ConnectAsync(timeoutMs, ct).ConfigureAwait(false);
        }
        catch (TimeoutException) { await stream.DisposeAsync().ConfigureAwait(false); return (PipeConnectResult.NotRunning, null); }
        catch (IOException) { await stream.DisposeAsync().ConfigureAwait(false); return (PipeConnectResult.NotRunning, null); }
        catch (UnauthorizedAccessException) { await stream.DisposeAsync().ConfigureAwait(false); return (PipeConnectResult.NotTrusted, null); }

        if (!Win32.GetNamedPipeServerProcessId(stream.SafePipeHandle, out var pid) ||
            !Win32.GetNamedPipeServerSessionId(stream.SafePipeHandle, out var session) ||
            session != ProcessInfo.CurrentSession ||
            ProcessInfo.SessionOf((int)pid) != ProcessInfo.CurrentSession ||
            !ProcessInfo.SamePath(ProcessInfo.ImagePath((int)pid), expectedServerImage))
        {
            await stream.DisposeAsync().ConfigureAwait(false);
            return (PipeConnectResult.NotTrusted, null);
        }
        return (PipeConnectResult.Ok, new PipeClient(stream) { ServerPid = (int)pid });
    }

    /// <summary>Sends one request line and reads one response line. Null when the server closed the pipe.</summary>
    public async Task<string?> RequestAsync(string line, CancellationToken ct = default)
    {
        await LineProtocol.WriteLineAsync(_stream, line.Replace("\r", " ").Replace("\n", " "), ct).ConfigureAwait(false);
        return await ReadLineAsync(ct).ConfigureAwait(false);
    }

    /// <summary>Buffered line read (Contracts LineReader), up to 16 MiB. Null at end of stream.</summary>
    public Task<string?> ReadLineAsync(CancellationToken ct = default) => _reader.ReadLineAsync(MaxResponseBytes, ct);

    public void Dispose() => _stream.Dispose();
    public ValueTask DisposeAsync() => _stream.DisposeAsync();
}
