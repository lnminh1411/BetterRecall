// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Collections.Concurrent;
using System.Text.Json;
using BetterRecall.Contracts;

namespace BetterRecall.Agent.Worker;

internal sealed record WorkerReply(WorkerResponse Header, byte[] Payload);

internal sealed class WorkerException(string message) : Exception(message);

/// <summary>
/// Client side of the worker frame protocol (v1-design section 4) over any duplex stream.
/// Requests carry increasing ids; one reader loop matches responses to pending requests by id.
/// </summary>
internal sealed class WorkerConnection : IAsyncDisposable
{
    private readonly Stream _stream;
    private readonly SemaphoreSlim _writeLock = new(1, 1);
    private readonly ConcurrentDictionary<long, TaskCompletionSource<WorkerReply>> _pending = new();
    private readonly CancellationTokenSource _cts = new();
    private readonly Task _reader;
    private long _nextId;
    private long _lastActivityTicks = Environment.TickCount64;
    private int _closed;

    /// <summary>Error (null on clean end of stream) and the number of requests still pending at close.</summary>
    public event Action<Exception?, int>? Closed;

    public WorkerConnection(Stream stream)
    {
        _stream = stream;
        _reader = Task.Run(ReadLoopAsync);
    }

    public int Pending => _pending.Count;
    public bool IsClosed => Volatile.Read(ref _closed) != 0;
    public long IdleMs => Environment.TickCount64 - Interlocked.Read(ref _lastActivityTicks);

    public async Task<WorkerReply> SendAsync(WorkerRequest request, ReadOnlyMemory<byte> payload, CancellationToken ct)
    {
        if (IsClosed) throw new WorkerException("Worker pipe closed.");
        long id = Interlocked.Increment(ref _nextId);
        var req = request with { Id = id };
        var tcs = new TaskCompletionSource<WorkerReply>(TaskCreationOptions.RunContinuationsAsynchronously);
        _pending[id] = tcs;
        Touch();
        try
        {
            var header = JsonSerializer.SerializeToUtf8Bytes(req, ContractsJson.Default.WorkerRequest);
            await _writeLock.WaitAsync(ct).ConfigureAwait(false);
            try
            {
                await BinaryFrame.WriteAsync(_stream, header, payload, ct).ConfigureAwait(false);
            }
            finally { _writeLock.Release(); }
        }
        catch (Exception ex)
        {
            _pending.TryRemove(id, out _);
            if (ex is IOException or ObjectDisposedException) Fail(ex);
            throw;
        }
        using var reg = ct.Register(() =>
        {
            if (_pending.TryRemove(id, out var t)) t.TrySetCanceled(ct);
        });
        var reply = await tcs.Task.ConfigureAwait(false);
        Touch();
        return reply;
    }

    private void Touch() => Interlocked.Exchange(ref _lastActivityTicks, Environment.TickCount64);

    private async Task ReadLoopAsync()
    {
        Exception? error = null;
        try
        {
            while (!_cts.IsCancellationRequested)
            {
                var frame = await BinaryFrame.ReadAsync(_stream, _cts.Token).ConfigureAwait(false);
                if (frame is null) break;
                var (headerBytes, payload) = frame.Value;
                WorkerResponse? header;
                try { header = JsonSerializer.Deserialize(headerBytes, ContractsJson.Default.WorkerResponse); }
                catch (JsonException) { throw new IOException("Bad worker frame header."); }
                if (header is null) throw new IOException("Empty worker frame header.");
                Touch();
                if (_pending.TryRemove(header.Id, out var tcs)) tcs.TrySetResult(new WorkerReply(header, payload));
            }
        }
        catch (OperationCanceledException) { }
        catch (Exception ex) { error = ex; }
        Fail(error);
    }

    private void Fail(Exception? error)
    {
        if (Interlocked.Exchange(ref _closed, 1) != 0) return;
        int pendingAtClose = _pending.Count;
        foreach (var kv in _pending)
            if (_pending.TryRemove(kv.Key, out var tcs)) tcs.TrySetException(new WorkerException("Worker pipe closed."));
        try { Closed?.Invoke(error, pendingAtClose); } catch { }
    }

    public async ValueTask DisposeAsync()
    {
        _cts.Cancel();
        try { _stream.Dispose(); } catch { }
        try { await _reader.ConfigureAwait(false); } catch { }
        Fail(null);
        _writeLock.Dispose();
    }
}
