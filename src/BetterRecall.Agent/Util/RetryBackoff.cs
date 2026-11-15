// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
namespace BetterRecall.Agent.Util;

/// <summary>
/// Exponential retry delay for a resource that failed (encoder, audio device): first retry after <c>initialMs</c>,
/// doubling per failure in a row up to <c>maxMs</c>; a success resets it. Times are milliseconds on any monotonic clock
/// (Environment.TickCount64 in the agent). Thread-safe.
/// </summary>
internal sealed class RetryBackoff(long initialMs = 1000, long maxMs = 60_000)
{
    private readonly object _gate = new();
    private int _failures;
    private long _nextAt = long.MinValue;

    /// <summary>Failures in a row since the last success.</summary>
    public int Failures { get { lock (_gate) return _failures; } }

    /// <summary>The clock value from which the next attempt may run.</summary>
    public long NextAt { get { lock (_gate) return _nextAt; } }

    /// <summary>True when no failure is pending or its delay has passed.</summary>
    public bool Due(long nowMs)
    {
        lock (_gate) return nowMs >= _nextAt;
    }

    /// <summary>Records a failure at nowMs and returns the delay before the next attempt.</summary>
    public long Failed(long nowMs)
    {
        lock (_gate)
        {
            _failures++;
            long delay = Math.Min(maxMs, initialMs << Math.Min(20, _failures - 1));
            _nextAt = nowMs + delay;
            return delay;
        }
    }

    public void Succeeded()
    {
        lock (_gate) { _failures = 0; _nextAt = long.MinValue; }
    }
}
