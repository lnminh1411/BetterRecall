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
    /// <param name="hasPendingInput">Checked when the idle timer fires: a request the agent sent at that moment is served
    /// instead of exiting under it (the agent would see a crash and back off).</param>
    public static async Task<LoopExit> RunAsync(Stream pipe, RequestHandler handler, TimeSpan idleExit, CancellationToken ct,
        Func<bool>? hasPendingInput = null)
    {
        while (true)
        {
            (byte[] Header, byte[] Payload)? frame;
            using (var idle = CancellationTokenSource.CreateLinkedTokenSource(ct))
            {
                idle.CancelAfter(idleExit);
                try
                {
                    frame = await BinaryFrame.ReadAsync(pipe, idle.Token).ConfigureAwait(false);
                }
                catch (OperationCanceledException)
                {
                    if (ct.IsCancellationRequested) return LoopExit.Cancelled;
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

            var reply = handler.Handle(frame.Value.Header, frame.Value.Payload);
            var header = JsonSerializer.SerializeToUtf8Bytes(reply.Header, ContractsJson.Default.WorkerResponse);
            try
            {
                await BinaryFrame.WriteAsync(pipe, header, reply.Payload, ct).ConfigureAwait(false);
            }
            catch (IOException) { return LoopExit.PipeClosed; }
            catch (ObjectDisposedException) { return LoopExit.PipeClosed; }
            catch (OperationCanceledException) { return LoopExit.Cancelled; }
            if (reply.Shutdown) return LoopExit.Shutdown;
        }
    }
}
