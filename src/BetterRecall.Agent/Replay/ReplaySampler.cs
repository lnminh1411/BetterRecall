// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using BetterRecall.Contracts;

namespace BetterRecall.Agent.Replay;

/// <summary>
/// RAM-only ring of encoded frames taken every replay.sampleSeconds while the replay buffer runs, so a saved replay can be
/// searched (capture kind replay) without decoding the video. Samples come from the keyframe grabs: same guardrails, same
/// downscale, same title filter. Samples older than the replay window are zeroed and dropped; panic and lock clear all.
/// </summary>
internal sealed class ReplaySampler
{
    internal sealed record Sample(DateTimeOffset Ts, int Monitor, string App, string Title, string Site, int Width, int Height, byte[] Bytes, string Mime);

    private readonly object _gate = new();
    private readonly List<Sample> _ring = [];
    private readonly Dictionary<int, DateTimeOffset> _last = [];
    private readonly Func<DateTimeOffset> _now;

    public ReplaySampler(Func<DateTimeOffset>? now = null) => _now = now ?? (() => DateTimeOffset.UtcNow);

    public int Count { get { lock (_gate) return _ring.Count; } }
    public long Bytes { get { lock (_gate) return _ring.Sum(s => (long)s.Bytes.Length); } }

    /// <summary>True when the monitor has no sample in the last sampleSeconds (minus a little slack for timer jitter).</summary>
    public bool Due(int monitor, int sampleSeconds)
    {
        lock (_gate)
            return !_last.TryGetValue(monitor, out var t) || _now() - t >= TimeSpan.FromSeconds(sampleSeconds) - TimeSpan.FromMilliseconds(400);
    }

    /// <summary>Keeps a copy of bytes; drops samples older than keepSeconds.</summary>
    public void Add(Sample s, int keepSeconds)
    {
        lock (_gate)
        {
            _ring.Add(s with { Bytes = s.Bytes.ToArray() });
            _last[s.Monitor] = s.Ts;
            Trim(keepSeconds);
        }
    }

    private void Trim(int keepSeconds)
    {
        var cutoff = _now() - TimeSpan.FromSeconds(Math.Max(15, keepSeconds));
        for (int i = _ring.Count - 1; i >= 0; i--)
        {
            if (_ring[i].Ts >= cutoff) continue;
            // Keep the newest old sample per monitor: on a still screen it is the frame the replay starts with. Not
            // forever: one from hours ago (before a pause or a purge) would join the next saved replay as offset 0.
            bool stale = _ring[i].Ts < cutoff - TimeSpan.FromSeconds(60);
            if (!stale && !_ring.Skip(i + 1).Any(x => x.Monitor == _ring[i].Monitor && x.Ts < cutoff)) continue;
            Array.Clear(_ring[i].Bytes);
            _ring.RemoveAt(i);
        }
    }

    /// <summary>
    /// Copies of the samples that belong to a clip [start, end]: every sample inside it, plus per monitor the last sample
    /// before start when the monitor has none in its first sampleSeconds (a still screen grabs nothing new).
    /// </summary>
    public List<Sample> Take(DateTimeOffset start, DateTimeOffset end, int sampleSeconds)
    {
        lock (_gate)
        {
            var list = _ring.Where(s => s.Ts >= start && s.Ts <= end).ToList();
            foreach (var mon in _ring.Select(s => s.Monitor).Distinct())
            {
                bool early = list.Any(s => s.Monitor == mon && s.Ts < start + TimeSpan.FromSeconds(sampleSeconds));
                if (early) continue;
                var before = _ring.Where(s => s.Monitor == mon && s.Ts < start).MaxBy(s => s.Ts);
                if (before is not null) list.Add(before with { Ts = start });
            }
            return list.OrderBy(s => s.Ts).ThenBy(s => s.Monitor).Select(s => s with { Bytes = s.Bytes.ToArray() }).ToList();
        }
    }

    public void Clear()
    {
        lock (_gate)
        {
            foreach (var s in _ring) Array.Clear(s.Bytes);
            _ring.Clear();
            _last.Clear();
        }
    }

    /// <summary>Frames for the store: offset from the clip start, clamped to the clip.</summary>
    public static IEnumerable<NewReplayFrame> ToFrames(string replayId, DateTimeOffset start, int durationMs, IEnumerable<Sample> samples) =>
        samples.Select(s => new NewReplayFrame(s.Ts, replayId, (int)Math.Clamp((s.Ts - start).TotalMilliseconds, 0, Math.Max(0, durationMs)),
            s.App, s.Title, s.Monitor, s.Width, s.Height, s.Bytes, s.Mime, s.Site));
}
