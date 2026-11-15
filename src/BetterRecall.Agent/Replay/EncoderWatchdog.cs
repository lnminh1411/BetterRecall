// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
namespace BetterRecall.Agent.Replay;

/// <summary>
/// Decides when a live replay encoder has failed (A7): it reported an error (MF event loop or VPL sync loop), or frames
/// went in (or waited on a busy encoder) for <see cref="StallMs"/> with no packet coming out. The pipeline always submits
/// at least one picture a second, so a healthy encoder never stays silent that long. Pure: the clock is passed in.
/// </summary>
internal sealed class EncoderWatchdog(long startMs, long stallMs = 3000)
{
    public long StallMs { get; } = stallMs;
    private long _lastPackets = -1;
    private long _lastProgressMs = startMs;
    private long _lastAttemptMs = long.MinValue;

    /// <summary>A frame was submitted, or could not be because the encoder was not ready.</summary>
    public void Attempt(long nowMs)
    {
        // After a gap with no attempts (replay paused, engine suspended) the silence is not the encoder's fault.
        if (_lastAttemptMs == long.MinValue || nowMs - _lastAttemptMs > StallMs)
            _lastProgressMs = Math.Max(_lastProgressMs, nowMs);
        _lastAttemptMs = nowMs;
    }

    /// <summary>The failure reason, or null while the encoder is healthy.</summary>
    public string? Check(long nowMs, long packetsOut, string? error)
    {
        if (!string.IsNullOrEmpty(error)) return error;
        if (packetsOut != _lastPackets)
        {
            _lastPackets = packetsOut;
            _lastProgressMs = nowMs;
            return null;
        }
        // Stalled: still trying to feed it, and nothing came out for StallMs.
        if (_lastAttemptMs >= _lastProgressMs && nowMs - _lastProgressMs >= StallMs)
            return $"no output for {(nowMs - _lastProgressMs) / 1000} s";
        return null;
    }
}
