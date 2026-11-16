// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Text.Json;
using BetterRecall.Contracts;
using BetterRecall.Worker.Hosting;

namespace BetterRecall.Worker.Protocol;

public enum LoopExit { PipeClosed, Shutdown, Idle, ProtocolError, Cancelled }

/// <summary>Reads frames one at a time, answers in arrival order. Exits on end of stream, shutdown, framing errors or
/// after <c>idleExit</c> with no request.</summary>
public static class WorkerLoop
{
    /// <summary>Keepalive (K review S1): while one request runs longer than this, an id-0 frame goes out at this interval.
    /// The agent counts any frame as alive, so an inline model compile or device re-bench is not taken for a hang.</summary>
    public static readonly TimeSpan KeepaliveEvery = TimeSpan.FromSeconds(20);
    /// <summary>Keepalives stop after this long on one request, so a handler stuck in native code still lets the agent's
    /// no-frame rule recycle the worker.</summary>
    public static readonly TimeSpan KeepaliveMax = TimeSpan.FromMinutes(15);

    /// <param name="hasPendingInput">Checked when the idle timer fires: a request the agent sent at that moment is served
    /// instead of exiting under it (the agent would see a crash and back off).</param>
    public static async Task<LoopExit> RunAsync(Stream pipe, RequestHandler handler, TimeSpan idleExit, CancellationToken ct,
        Func<bool>? hasPendingInput = null, TimeSpan? keepaliveEvery = null, TimeSpan? keepaliveMax = null)
    {
        // Keepalive and reply share the pipe: one frame at a time, never interleaved.
        using var writeLock = new SemaphoreSlim(1, 1);
        var keepalive = JsonSerializer.SerializeToUtf8Bytes(new WorkerResponse(0, true), ContractsJson.Default.WorkerResponse);
        while (true)
        {
            (byte[] Header, byte[] Payload)? frame;
            using (var idle = CancellationTokenSource.CreateLinkedTokenSource(ct))
            {
                idle.CancelAfter(idleExit);
                try
                {
                    // The idle timer only covers the wait for a frame's first byte: a frame that has started is read to
                    // its end, so a timer firing mid-frame cannot leave the stream out of step.
                    frame = await BinaryFrame.ReadAsync(pipe, ct, idle.Token).ConfigureAwait(false);
                }
                catch (OperationCanceledException)
                {
                    if (ct.IsCancellationRequested) return LoopExit.Cancelled;
                    // No byte of a frame was consumed, so a request that arrived just now is still whole in the pipe.
                    if (hasPendingInput?.Invoke() == true) { Log.Info("idle exit skipped: a request arrived"); continue; }
                    Log.Info($"idle for {idleExit.TotalSeconds:F0} s, exiting");
                    return LoopExit.Idle;
                }
                catch (EndOfStreamException) { return LoopExit.PipeClosed; }
                catch (IOException e)
                {
                    Log.Warn($"framing error: {e.Message}");
                    return LoopExit.ProtocolError;
                }
                catch (OverflowException)
                {
                    Log.Warn("framing error: length overflow");
                    return LoopExit.ProtocolError;
                }
            }
            if (frame is null) return LoopExit.PipeClosed;

            WorkerReply reply;
            using (var stopKeepalive = new CancellationTokenSource())
            {
                var beats = KeepaliveAsync(pipe, writeLock, keepalive, keepaliveEvery ?? KeepaliveEvery, keepaliveMax ?? KeepaliveMax, stopKeepalive.Token);
                try { reply = handler.Handle(frame.Value.Header, frame.Value.Payload); }
                finally
                {
                    stopKeepalive.Cancel();
                    await beats.ConfigureAwait(false);
                }
            }
            var header = JsonSerializer.SerializeToUtf8Bytes(reply.Header, ContractsJson.Default.WorkerResponse);
            if (header.Length > BinaryFrame.MaxHeaderBytes || reply.Payload.Length > BinaryFrame.MaxPayloadBytes)
            {
                // C1: WriteAsync would throw before sending a byte and the worker used to exit without a reply. Answer the
                // request with an error instead: the pipe stays in step and the agent drops just this frame.
                Log.Warn($"reply too large for a frame (header {header.Length} B, payload {reply.Payload.Length} B): internal error sent");
                header = JsonSerializer.SerializeToUtf8Bytes(new WorkerResponse(reply.Header.Id, false, WorkerErrors.Internal), ContractsJson.Default.WorkerResponse);
                reply = reply with { Payload = [] };
            }
            try
            {
                await writeLock.WaitAsync(ct).ConfigureAwait(false);
                try { await BinaryFrame.WriteAsync(pipe, header, reply.Payload, ct).ConfigureAwait(false); }
                finally { writeLock.Release(); }
            }
            catch (IOException) { return LoopExit.PipeClosed; }
            catch (ObjectDisposedException) { return LoopExit.PipeClosed; }
            catch (OperationCanceledException) { return LoopExit.Cancelled; }
            if (reply.Shutdown) return LoopExit.Shutdown;
        }
    }

    /// <summary>Sends id-0 frames every <paramref name="every"/> until stopped or <paramref name="max"/> has passed. A frame
    /// once begun is written whole (no token), so stopping cannot leave the pipe out of step; a broken pipe just ends it
    /// and the reply write reports the error.</summary>
    private static async Task KeepaliveAsync(Stream pipe, SemaphoreSlim writeLock, byte[] header, TimeSpan every, TimeSpan max, CancellationToken stop)
    {
        long until = Environment.TickCount64 + (long)max.TotalMilliseconds;
        try
        {
            while (true)
            {
                await Task.Delay(every, stop).ConfigureAwait(false);
                if (Environment.TickCount64 >= until) { Log.Warn($"request still running after {max.TotalMinutes:F0} min: keepalive stopped"); return; }
                await writeLock.WaitAsync(stop).ConfigureAwait(false);
                try { await BinaryFrame.WriteAsync(pipe, header, ReadOnlyMemory<byte>.Empty, CancellationToken.None).ConfigureAwait(false); }
                finally { writeLock.Release(); }
            }
        }
        catch (OperationCanceledException) { }
        catch (IOException) { }
        catch (ObjectDisposedException) { }
    }
}
