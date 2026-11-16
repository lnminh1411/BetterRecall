// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
namespace BetterRecall.Agent.Capture;

/// <summary>Keyframe encode levels, cheapest last. Normal and Fast differ by encoder speed; Reduced also scales the
/// picture down and lowers quality; WebP is the last resort for frames that waited too long.</summary>
internal enum EncodeLevel { Normal, Fast, Reduced, WebP }

/// <summary>
/// CPU budget for keyframe encoding (user requirement, 2026-11-12): as Task Manager counts CPU, i.e. a share of all
/// logical processors, the encode work may use under 1 % on average over 60 s and at most 1.5 % in any 10 s.
/// Every encode reports the CPU it used; before each one the load of the fuller window picks the level (see Choose),
/// and the level's expected cost must still fit both windows with a safety margin. When none fits, kept frames wait in a small queue (oldest first; the newest
/// is never dropped). A full queue, or a frame waiting longer than MaxWaitMs, sends the oldest frame through WebP when
/// that fits, else drops it. Pure logic: clock and encode callback are injected. Not thread-safe; one owner.
/// </summary>
internal sealed class EncodeGovernor<T> where T : IDisposable
{
    public const double AvgPercent = 1.0, PeakPercent = 1.5;
    /// <summary>Plan against these shares of each limit so that one frame over its estimate cannot break the limit.</summary>
    public const double Window10Target = 0.8, Window60Target = 0.9;
    public const int QueueCapacity = 6;
    public const long MaxWaitMs = 30_000;

    private readonly Func<long> _clockMs;
    private readonly Func<T, EncodeLevel, double> _encode;
    private readonly Action<T, string> _drop;
    private readonly Queue<(long T, double Ms)> _samples = new();
    private readonly Queue<(T Item, long Since)> _waiting = new();
    private readonly double[] _estimate;
    private readonly double[] _initial;
    private readonly long[] _lastRun;
    /// <summary>A level unused this long gets half its estimate's excess over the initial value back (A12).</summary>
    public const long RelaxAfterMs = 60_000;
    private double _used10, _used60;

    public double Limit10Ms { get; }
    public double Limit60Ms { get; }
    /// <summary>Level of the last encode (Normal until the first one).</summary>
    public EncodeLevel Level { get; private set; }
    public int Waiting => _waiting.Count;
    public long Encoded, Deferred, Dropped, WebPFallbacks, Downgraded;
    /// <summary>Replay sample encodes skipped because they did not fit the budget (A19).</summary>
    public long SamplesDeferred;
    private double _sampleEstimate = -1;

    /// <param name="encode">Encodes and stores the item at the level; returns the CPU ms it used. Owns nothing: the
    /// governor disposes the item after the call.</param>
    /// <param name="drop">Called before a dropped item is disposed (reason for the log).</param>
    /// <param name="initialEstimateMs">Expected CPU ms per level until real encodes have been measured.</param>
    public EncodeGovernor(Func<long> clockMs, int logicalProcessors, Func<T, EncodeLevel, double> encode, Action<T, string> drop,
        double[] initialEstimateMs)
    {
        if (initialEstimateMs.Length != 4) throw new ArgumentException("one estimate per level", nameof(initialEstimateMs));
        _clockMs = clockMs;
        _encode = encode;
        _drop = drop;
        _estimate = (double[])initialEstimateMs.Clone();
        _initial = (double[])initialEstimateMs.Clone();
        long t0 = clockMs();
        _lastRun = [t0, t0, t0, t0];
        int procs = Math.Max(1, logicalProcessors);
        Limit10Ms = PeakPercent / 100 * procs * 10_000;
        Limit60Ms = AvgPercent / 100 * procs * 60_000;
    }

    /// <summary>CPU ms of encoding in the last 10 s and 60 s.</summary>
    public (double Last10, double Last60) Used
    {
        get { Prune(_clockMs()); return (_used10, _used60); }
    }

    public double Estimate(EncodeLevel level) => _estimate[(int)level];

    /// <summary>A new keyframe to store. Encoded now when the budget allows and nothing is waiting, else queued.</summary>
    public void Offer(T item)
    {
        _waiting.Enqueue((item, _clockMs()));
        Pump();
        foreach (var w in _waiting)
            if (ReferenceEquals(w.Item, item)) { Deferred++; break; }
    }

    /// <summary>Encodes waiting frames while the budget allows; handles a full or stale queue. Call often (each loop).</summary>
    public void Pump()
    {
        while (_waiting.Count > 0)
        {
            long now = _clockMs();
            Prune(now);
            var (item, since) = _waiting.Peek();
            if (Choose() is { } level)
            {
                _waiting.Dequeue();
                Run(item, level);
                continue;
            }
            bool full = _waiting.Count > QueueCapacity, stale = now - since > MaxWaitMs;
            if (!full && !stale) return;
            _waiting.Dequeue();
            if (Fits(EncodeLevel.WebP))
            {
                WebPFallbacks++;
                Run(item, EncodeLevel.WebP);
            }
            else
            {
                Dropped++;
                try { _drop(item, full ? "queue full" : "waited too long"); }
                finally { item.Dispose(); }
            }
        }
    }

    /// <summary>Load: the fuller of the two windows as a share of its target (0 idle, 1 at the target).</summary>
    public double Load
    {
        get
        {
            Prune(_clockMs());
            return Math.Max(_used10 / (Window10Target * Limit10Ms), _used60 / (Window60Target * Limit60Ms));
        }
    }

    /// <summary>The AVIF level for the next frame, or null (wait). The load picks the starting level (Normal below half the
    /// target, Fast below three quarters, else Reduced), so a sustained overload settles on cheaper encodes instead of
    /// spending the window on Normal and then stalling; a level that does not fit falls through to the cheaper ones.</summary>
    public EncodeLevel? Choose()
    {
        Relax(_clockMs());
        double load = Load;
        var start = load < 0.5 ? EncodeLevel.Normal : load < 0.75 ? EncodeLevel.Fast : EncodeLevel.Reduced;
        for (var l = start; l <= EncodeLevel.Reduced; l++)
            if (Fits(l)) return l;
        return null;
    }

    /// <summary>A level's estimate is updated only when it runs, and a level whose estimate alone is over the target
    /// never runs: one slow encode (first after start, a stall under memory pressure) used to disable it for good (A12).
    /// An unused level therefore drifts back toward its initial estimate, halving the excess each RelaxAfterMs, and is
    /// measured again once it fits. The windows still charge what the encode really cost.</summary>
    private void Relax(long now)
    {
        for (int i = 0; i < _estimate.Length; i++)
        {
            if (now - _lastRun[i] < RelaxAfterMs) continue;
            _lastRun[i] = now;
            if (_estimate[i] > _initial[i]) _estimate[i] = (_estimate[i] + _initial[i]) / 2;
        }
    }

    private bool Fits(EncodeLevel l) =>
        _used10 + _estimate[(int)l] <= Window10Target * Limit10Ms && _used60 + _estimate[(int)l] <= Window60Target * Limit60Ms;

    /// <summary>Counts CPU spent outside the queue (the replay sample encode, also stored as the keyframe when the
    /// formats match) against the same windows, so the budget covers every keyframe-related encode.</summary>
    /// <summary>A replay sample encode may run now: its expected cost fits both windows and no kept keyframe is waiting
    /// for budget (keyframes come first). Samples used to be charged only after the fact, so on a small CPU with large
    /// frames they alone could exceed the limits and starve keyframes (A19). A false answer is counted; the caller skips
    /// this sample and the next one is due a few seconds later.</summary>
    public bool TryAdmitSample()
    {
        Prune(_clockMs());
        double est = _sampleEstimate >= 0 ? _sampleEstimate : _estimate[(int)EncodeLevel.WebP];
        bool fits = _waiting.Count == 0 && _used10 + est <= Window10Target * Limit10Ms && _used60 + est <= Window60Target * Limit60Ms;
        if (!fits) SamplesDeferred++;
        return fits;
    }

    /// <summary>Charges a replay sample encode and learns its cost for <see cref="TryAdmitSample"/>.</summary>
    public void ChargeSample(double ms)
    {
        ms = Math.Max(0, ms);
        _sampleEstimate = _sampleEstimate < 0 ? ms : _sampleEstimate * 0.7 + ms * 0.3;
        Charge(ms);
    }

    public void Charge(double ms)
    {
        if (ms <= 0) return;
        long now = _clockMs();
        Prune(now);
        _samples.Enqueue((now, ms));
        _used10 += ms;
        _used60 += ms;
    }

    private void Run(T item, EncodeLevel level)
    {
        double ms;
        try { ms = Math.Max(0, _encode(item, level)); }
        finally { item.Dispose(); }
        long now = _clockMs();
        _samples.Enqueue((now, ms));
        _used10 += ms;
        _used60 += ms;
        ref double e = ref _estimate[(int)level];
        e = e * 0.7 + ms * 0.3;              // follows the content; one slow frame moves it only a little
        _lastRun[(int)level] = now;
        if (level != EncodeLevel.Normal) Downgraded++;
        Encoded++;
        Level = level;
    }

    private void Prune(long now)
    {
        // _used10 is recomputed from the samples (at most a few hundred in 60 s); _used60 drops what left the window.
        while (_samples.Count > 0 && now - _samples.Peek().T >= 60_000) _used60 -= _samples.Dequeue().Ms;
        if (_samples.Count == 0) _used60 = 0;
        double s10 = 0;
        foreach (var (t, ms) in _samples) if (now - t < 10_000) s10 += ms;
        _used10 = s10;
    }

    /// <summary>Disposes waiting frames (shutdown).</summary>
    public void Clear()
    {
        while (_waiting.Count > 0) _waiting.Dequeue().Item.Dispose();
    }
}
