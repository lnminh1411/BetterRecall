// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
namespace BetterRecall.Agent.Audio;

/// <summary>
/// Sample clock for the replay audio track. A run is a stretch of continuous audio whose time is
/// run start (QPC) + frames fed / sample rate, so AAC packet times never drift from the video clock.
/// Silence inside a run is encoded as zeros so the timeline stays exact; after <see cref="MaxSilence100ns"/>
/// of silence the run ends and an idle desktop costs no encoder work. Gaps from late packets are filled,
/// overlap from clock drift is trimmed, jumps beyond <see cref="MaxSilence100ns"/> start a new run.
/// </summary>
internal sealed class AudioTimeline(int sampleRate)
{
    public const long MaxSilence100ns = 20_000_000;   // 2 s
    public const long Jitter100ns = 20_000;           // 2 ms: treat as continuous

    private long _runStart = long.MinValue;
    private long _framesFed;
    private long _lastLoudEnd;

    public bool InRun => _runStart != long.MinValue;
    public long RunStart => _runStart;
    public long Runs { get; private set; }
    public long FilledFrames { get; private set; }
    public long TrimmedFrames { get; private set; }

    /// <summary>Time of the next frame to feed, 100 ns QPC.</summary>
    public long Expected => _runStart + _framesFed * 10_000_000L / sampleRate;

    public readonly record struct Step(bool EndRun, bool StartRun, long FillFrames, int SkipFrames, int FeedFrames, bool FeedSilent);

    /// <summary>Plans one captured packet. Call <see cref="Commit"/> with the step after acting on it.</summary>
    public Step Plan(long qpc, int frames, bool silent)
    {
        if (frames <= 0) return default;
        bool end = false;
        if (InRun)
        {
            long gap = qpc - Expected;
            if (gap > MaxSilence100ns || gap < -MaxSilence100ns) end = true;
            else if (silent && Expected - _lastLoudEnd >= MaxSilence100ns) return new Step(true, false, 0, 0, 0, false);
            else if (gap > Jitter100ns) return new Step(false, false, gap * sampleRate / 10_000_000L, 0, frames, silent);
            else if (gap < -Jitter100ns)
            {
                int skip = (int)Math.Min(frames, -gap * sampleRate / 10_000_000L);
                return new Step(false, false, 0, skip, frames - skip, silent);
            }
            else return new Step(false, false, 0, 0, frames, silent);
        }
        if (silent) return new Step(end, false, 0, 0, 0, false);
        return new Step(end, true, 0, 0, frames, false);
    }

    public void Commit(Step s, long qpc)
    {
        if (s.EndRun) { _runStart = long.MinValue; _framesFed = 0; }
        if (s.StartRun) { _runStart = qpc; _framesFed = 0; _lastLoudEnd = qpc; Runs++; }
        if (!InRun) return;
        _framesFed += s.FillFrames + s.FeedFrames;
        FilledFrames += s.FillFrames;
        TrimmedFrames += s.SkipFrames;
        if (s.FeedFrames > 0 && !s.FeedSilent) _lastLoudEnd = Expected;
    }
}
