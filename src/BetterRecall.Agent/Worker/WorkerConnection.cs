// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Collections.Concurrent;
using System.Runtime.InteropServices;
using System.Text.Json;
using BetterRecall.Contracts;

namespace BetterRecall.Agent.Worker;

internal sealed record WorkerReply(WorkerResponse Header, byte[] Payload);

internal class WorkerException(string message) : Exception(message);

/// <summary>The worker answered this request with an error (bad input, model not installed). Retrying the same request
/// gives the same answer, unlike a <see cref="WorkerException"/> for a closed pipe or a restart.</summary>
internal sealed class WorkerReplyException(string message) : WorkerException(message)
{
    public bool ModelMissing => Message.StartsWith("Model not installed", StringComparison.Ordinal);
}

/// <summary>
/// Client side of the worker frame protocol (v1-design section 4) over any duplex stream.
/// Requests carry increasing ids; one reader loop matches responses to pending requests by id.
/// </summary>
internal sealed class WorkerConnection : IAsyncDisposable
{
    private readonly Stream _stream;
    private readonly WriteGate _writeLock = new();
    // Liveness clock in ms (C8): skips time the PC slept, so a sleep is not taken for a silent worker.
    private readonly Func<long> _now;
    private readonly ConcurrentDictionary<long, TaskCompletionSource<WorkerReply>> _pending = new();
    private readonly CancellationTokenSource _cts = new();
    private readonly Task _reader;
    private long _nextId;
    private long _lastActivityTicks = Environment.TickCount64;
    private long _lastReplyTicks;
    private int _consecutiveTimeouts;
    private int _longOps;
    private int _closed;
    private int _disposed;
    // Liveness (A5, CA-S1): when each request still waiting for its reply began, and the requests a caller gave up on
    // that the serial worker still serves, until their own late reply comes (K review N1: a cancel during the write
    // counts too, since the write goes on, A2). Keepalive frames (id 0) do not clear them.
    private readonly ConcurrentDictionary<long, (string Op, long Ticks)> _started = new();
    private readonly Lock _abandonGate = new();
    private readonly Dictionary<long, (string Op, long Ticks)> _abandoned = new();

    /// <summary>Error (null on clean end of stream) and the number of requests still pending at close.</summary>
    public event Action<Exception?, int>? Closed;

    /// <param name="clock">Liveness clock in ms; default <see cref="LivenessClock.NowMs"/>, which stops while the PC
    /// sleeps (tests inject one).</param>
    public WorkerConnection(Stream stream, Func<long>? clock = null)
    {
        _stream = stream;
        _now = clock ?? LivenessClock.NowMs;
        _lastReplyTicks = _now();
        _reader = Task.Run(ReadLoopAsync);
    }

    public int Pending => _pending.Count;
    public bool IsClosed => Volatile.Read(ref _closed) != 0;
    public long IdleMs => Environment.TickCount64 - Interlocked.Read(ref _lastActivityTicks);
    /// <summary>Time since the worker last sent any frame (sends do not count, unlike IdleMs).</summary>
    public long NoReplyMs => _now() - Interlocked.Read(ref _lastReplyTicks);
    /// <summary>Requests whose caller gave up before the reply, since the last frame from the worker.</summary>
    public int ConsecutiveTimeouts => Volatile.Read(ref _consecutiveTimeouts);
    /// <summary>A request that may legitimately run for minutes (bench) is in flight: replies to the others wait for it.</summary>
    public bool LongOpPending => Volatile.Read(ref _longOps) > 0;
    /// <summary>The oldest request the worker owes a reply (waiting, or abandoned but still served) and how long
    /// the worker has owed it: since it began or since the worker's last frame, whichever is later. Null when nothing is
    /// owed. Untimed callers (scheduler, transcriber) count too: the old rule needed callers that time out.</summary>
    public (string Op, long Ms)? Oldest
    {
        get
        {
            (string Op, long Ticks)? oldest;
            oldest = null;
            lock (_abandonGate)
                foreach (var v in _abandoned.Values)
                    if (oldest is null || v.Ticks < oldest.Value.Ticks) oldest = v;
            foreach (var kv in _started)
                if (oldest is null || kv.Value.Ticks < oldest.Value.Ticks) oldest = kv.Value;
            if (oldest is not { } o) return null;
            return (o.Op, Math.Min(_now() - o.Ticks, NoReplyMs));
        }
    }

    /// <summary>The request the serial worker is serving: the oldest one it owes (waiting or abandoned). Null when none.</summary>
    private long? ServingId()
    {
        long? id = null;
        long ticks = long.MaxValue;
        lock (_abandonGate)
            foreach (var (k, v) in _abandoned)
                if (v.Ticks < ticks || v.Ticks == ticks && k < id) { id = k; ticks = v.Ticks; }
        foreach (var kv in _started)
            if (kv.Value.Ticks < ticks || kv.Value.Ticks == ticks && kv.Key < id) { id = kv.Key; ticks = kv.Value.Ticks; }
        return id;
    }

    /// <summary>Set by the supervisor when it kills this worker for not answering; the failure is already counted.</summary>
    public bool Recycled { get; set; }

    /// <summary>The supervisor sent shutdown: the worker may close the pipe before it answers, which is not a crash.</summary>
    public bool ShuttingDown { get; set; }

    /// <summary>The agent stopped this worker on purpose (a cancelled bench): no request in flight is to blame.</summary>
    public bool Aborted { get; set; }

    /// <summary>Message of the requests the agent's own stop fails (abort, shutdown, or a liveness recycle of a worker
    /// serving another request). Unlike "Worker pipe closed." it does not count as a crash on that job (A-L1, C8).</summary>
    public const string StoppedMessage = "Worker stopped by the agent.";

    /// <summary>The connection closed before this request was written: the worker never saw it (not a crash on it).</summary>
    public const string NotSentMessage = "Worker connection closed; request not sent.";

    internal bool IsDisposed => Volatile.Read(ref _disposed) != 0;

    private static bool IsLongOp(string op) => op is "bench" or "hello";

    /// <param name="urgent">An interactive request (search query embed): its write goes before every waiting normal one,
    /// right after the frame being written now. The op the worker is running cannot be preempted (A-L4).</param>
    public async Task<WorkerReply> SendAsync(WorkerRequest request, ReadOnlyMemory<byte> payload, CancellationToken ct, bool urgent = false)
    {
        if (IsClosed) throw new WorkerException(NotSentMessage);
        long id = Interlocked.Increment(ref _nextId);
        var req = request with { Id = id };
        var tcs = new TaskCompletionSource<WorkerReply>(TaskCreationOptions.RunContinuationsAsynchronously);
        _pending[id] = tcs;
        _started[id] = (request.Op, _now());
        Touch();
        bool longOp = IsLongOp(request.Op);
        if (longOp) Interlocked.Increment(ref _longOps);
        try { return await SendCoreAsync(id, tcs, req, payload, ct, urgent).ConfigureAwait(false); }
        finally
        {
            _started.TryRemove(id, out _);
            if (longOp) Interlocked.Decrement(ref _longOps);
        }
    }

    private async Task<WorkerReply> SendCoreAsync(long id, TaskCompletionSource<WorkerReply> tcs, WorkerRequest req, ReadOnlyMemory<byte> payload, CancellationToken ct, bool urgent)
    {
        try
        {
            var header = JsonSerializer.SerializeToUtf8Bytes(req, ContractsJson.Default.WorkerRequest);
            // Too big for a frame: BinaryFrame.WriteAsync would throw before writing a byte, so the pipe is fine.
            if (header.Length > BinaryFrame.MaxHeaderBytes || payload.Length > BinaryFrame.MaxPayloadBytes)
                throw new WorkerException("Request too large.");
            await _writeLock.EnterAsync(urgent, ct).ConfigureAwait(false);   // a cancel here leaves nothing half sent
            // A2: once started, the frame is written whole, without the caller's token. On the 0-byte worker pipe a write
            // waits while the serial worker is busy (a cold transcribe takes minutes): cancelling it there closed the
            // pipe, failed the request in flight and counted a crash. A caller that gives up only abandons the reply.
            var write = WriteFrameAsync(header, payload);
            try { await write.WaitAsync(ct).ConfigureAwait(false); }
            catch (OperationCanceledException)
            {
                // Owed from now: its reply cannot come before the write ends. A failed write closes the connection.
                if (_started.TryGetValue(id, out var st)) lock (_abandonGate) _abandoned[id] = st;
                _ = write.ContinueWith(static t => _ = t.Exception, CancellationToken.None, TaskContinuationOptions.OnlyOnFaulted | TaskContinuationOptions.ExecuteSynchronously, TaskScheduler.Default);
                throw;
            }
        }
        catch (Exception)
        {
            _pending.TryRemove(id, out _);
            throw;
        }
        using var reg = ct.Register(() =>
        {
            TaskCompletionSource<WorkerReply>? t;
            // Under the gate with the reader's removal: a reply racing this cancel cannot leave a stale entry behind.
            lock (_abandonGate)
            {
                if (!_pending.TryRemove(id, out t)) return;
                if (_started.TryGetValue(id, out var st)) _abandoned[id] = st;
            }
            Interlocked.Increment(ref _consecutiveTimeouts);
            t.TrySetCanceled(ct);
        });
        var reply = await tcs.Task.ConfigureAwait(false);
        Touch();
        return reply;
    }

    /// <summary>Writes one frame under the write lock the caller took, and releases it. A failed write (broken pipe, or a
    /// frame left part-way) leaves the pipe out of step: every later reply would be garbage, so the connection closes and
    /// the supervisor starts a fresh worker.</summary>
    private async Task WriteFrameAsync(byte[] header, ReadOnlyMemory<byte> payload)
    {
        try { await BinaryFrame.WriteAsync(_stream, header, payload, CancellationToken.None).ConfigureAwait(false); }
        catch (Exception ex)
        {
            Fail(ex);
            // A-L1: the caller used to get the raw IOException / ObjectDisposedException, which the scheduler took for a
            // local failure and set the job aside for good. The worker never read this frame: never a crash on it.
            throw new WorkerException(Recycled || ShuttingDown || Aborted ? StoppedMessage : NotSentMessage);
        }
        finally { _writeLock.Exit(); }
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
                Interlocked.Exchange(ref _lastReplyTicks, _now());
                Interlocked.Exchange(ref _consecutiveTimeouts, 0);   // any frame, late replies to cancelled requests too: it is alive
                // A keepalive (id 0) only says the worker is still on its request: an abandoned one stays owed until its
                // own reply, so if the keepalives stop (they end after 15 min) the silence counts from the last one.
                TaskCompletionSource<WorkerReply>? tcs;
                lock (_abandonGate)
                {
                    _abandoned.Remove(header.Id);
                    _pending.TryRemove(header.Id, out tcs);
                }
                tcs?.TrySetResult(new WorkerReply(header, payload));
            }
        }
        catch (OperationCanceledException) { }
        catch (Exception ex) { error = ex; }
        Fail(error);
    }

    private void Fail(Exception? error)
    {
        if (Interlocked.Exchange(ref _closed, 1) != 0) return;
        _writeLock.Close();
        int pendingAtClose = _pending.Count;
        // A-L1/C8: "Worker pipe closed." (a crash on that job for the scheduler) only for a request the worker may have
        // died on: every request when it went away by itself; when the agent recycled it for not answering, only the one
        // it was serving. Requests queued behind, and all of them on an abort or a shutdown, get StoppedMessage.
        bool byAgent = Recycled || ShuttingDown || Aborted;
        long? blamed = Recycled && !Aborted && !ShuttingDown ? ServingId() : null;
        foreach (var kv in _pending)
            if (_pending.TryRemove(kv.Key, out var tcs))
                tcs.TrySetException(new WorkerException(!byAgent || kv.Key == blamed ? "Worker pipe closed." : StoppedMessage));
        try { Closed?.Invoke(error, pendingAtClose); } catch { }
    }

    /// <summary>Idempotent: the supervisor disposes on every close (C10); recycle, abort and shutdown dispose too.</summary>
    public async ValueTask DisposeAsync()
    {
        if (Interlocked.Exchange(ref _disposed, 1) != 0) return;
        _cts.Cancel();
        try { _stream.Dispose(); } catch { }
        try { await _reader.ConfigureAwait(false); } catch { }
        Fail(null);
        _cts.Dispose();
    }

    /// <summary>The write lock in two lanes (A-L4): a waiting urgent writer gets it before every waiting normal one.
    /// A cancelled wait leaves its lane; closing fails every waiter with NotSentMessage (the worker never saw them).</summary>
    private sealed class WriteGate
    {
        private readonly Lock _g = new();
        private readonly LinkedList<TaskCompletionSource> _urgent = new(), _normal = new();
        private bool _held, _closed;

        public Task EnterAsync(bool urgent, CancellationToken ct)
        {
            TaskCompletionSource tcs;
            LinkedListNode<TaskCompletionSource> node;
            lock (_g)
            {
                if (_closed) return Task.FromException(new WorkerException(NotSentMessage));
                if (ct.IsCancellationRequested) return Task.FromCanceled(ct);
                if (!_held) { _held = true; return Task.CompletedTask; }
                tcs = new TaskCompletionSource(TaskCreationOptions.RunContinuationsAsynchronously);
                node = (urgent ? _urgent : _normal).AddLast(tcs);
            }
            if (!ct.CanBeCanceled) return tcs.Task;
            var reg = ct.Register(() =>
            {
                lock (_g)
                {
                    if (node.List is not { } list) return;   // granted or failed already
                    list.Remove(node);
                }
                tcs.TrySetCanceled(ct);
            });
            _ = tcs.Task.ContinueWith(static (_, r) => ((CancellationTokenRegistration)r!).Dispose(), reg,
                CancellationToken.None, TaskContinuationOptions.ExecuteSynchronously, TaskScheduler.Default);
            return tcs.Task;
        }

        public void Exit()
        {
            TaskCompletionSource next;
            lock (_g)
            {
                var lane = _urgent.Count > 0 ? _urgent : _normal.Count > 0 ? _normal : null;
                if (lane is null) { _held = false; return; }
                next = lane.First!.Value;
                lane.RemoveFirst();
            }
            next.TrySetResult();
        }

        public void Close()
        {
            TaskCompletionSource[] waiting;
            lock (_g)
            {
                _closed = true;
                waiting = [.. _urgent, .. _normal];
                _urgent.Clear();
                _normal.Clear();
            }
            foreach (var t in waiting) t.TrySetException(new WorkerException(NotSentMessage));
        }
    }
}

/// <summary>Milliseconds of QueryUnbiasedInterruptTime: like TickCount64 but without the time the PC spent asleep or
/// hibernating (C8). Worker liveness runs on it, so a worker frozen by a sleep has not been silent for the sleep.</summary>
internal static partial class LivenessClock
{
    public static long NowMs() => QueryUnbiasedInterruptTime(out ulong t) ? (long)(t / 10_000) : Environment.TickCount64;

    [LibraryImport("kernel32.dll")]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static partial bool QueryUnbiasedInterruptTime(out ulong unbiasedTime);
}
