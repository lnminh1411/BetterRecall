// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Runtime.InteropServices;

namespace BetterRecall.Agent.Audio;

/// <summary>Raw PCM lists are zeroed, not just emptied: List&lt;float&gt;.Clear and RemoveRange only move the count, so
/// the samples stayed in the backing array until the GC reused it (CA-S2).</summary>
internal static class Pcm
{
    /// <summary>Zeroes the whole backing array, then empties the list.</summary>
    public static void Wipe(List<float> l)
    {
        CollectionsMarshal.SetCount(l, l.Capacity);
        CollectionsMarshal.AsSpan(l).Clear();
        l.Clear();
    }

    /// <summary>RemoveRange(0, n) that zeroes the vacated tail.</summary>
    public static void RemoveHead(List<float> l, int n)
    {
        if (n <= 0) return;
        int old = l.Count;
        l.RemoveRange(0, n);
        int now = l.Count;
        CollectionsMarshal.SetCount(l, old);
        CollectionsMarshal.AsSpan(l)[now..].Clear();
        CollectionsMarshal.SetCount(l, now);
    }

    /// <summary>AddRange that grows into a new list itself and wipes the old backing array.</summary>
    public static List<float> Append(List<float> l, ReadOnlySpan<float> data)
    {
        if (l.Count + data.Length > l.Capacity)
        {
            var n = new List<float>(Math.Max(l.Capacity * 2, l.Count + data.Length));
            n.AddRange(CollectionsMarshal.AsSpan(l));
            Wipe(l);
            l = n;
        }
        l.AddRange(data);
        return l;
    }
}

/// <summary>Converts interleaved PCM to 16 kHz mono float32.</summary>
internal sealed class Resampler
{
    private readonly int _inRate;
    private readonly int _channels;
    private readonly bool _float;
    private readonly int _bits;
    private double _pos;      // fractional read position into _mono
    private readonly List<float> _mono = new();

    public const int OutRate = 16000;

    public Resampler(PcmFormat fmt)
    {
        _inRate = fmt.SampleRate;
        _channels = fmt.Channels;
        _float = fmt.Float;
        _bits = fmt.BitsPerSample;
    }

    public void Reset() { _pos = 0; Pcm.Wipe(_mono); }

    /// <summary>Appends resampled output for this chunk to output. Leftover input carries to the next call.</summary>
    public void Process(ReadOnlySpan<byte> data, int frames, bool silent, List<float> output)
    {
        for (int f = 0; f < frames; f++)
        {
            float sum = 0;
            if (!silent)
            {
                for (int c = 0; c < _channels; c++)
                {
                    int i = f * _channels + c;
                    sum += _float
                        ? BitConverter.ToSingle(data.Slice(i * 4, 4))
                        : _bits == 16 ? BitConverter.ToInt16(data.Slice(i * 2, 2)) / 32768f
                        : (BitConverter.ToInt32(data.Slice(i * 4, 4)) >> 8) / 8388608f;
                }
            }
            _mono.Add(sum / _channels);
        }
        if (_inRate == OutRate)
        {
            output.AddRange(_mono);
            Pcm.Wipe(_mono);
            return;
        }
        double step = (double)_inRate / OutRate;
        int ratio = (int)Math.Round(step);
        bool integer = Math.Abs(step - ratio) < 1e-9 && ratio >= 1;
        while (true)
        {
            int i0 = (int)Math.Floor(_pos);
            if (integer)
            {
                if (i0 + ratio > _mono.Count) break;
                float s = 0;
                for (int k = 0; k < ratio; k++) s += _mono[i0 + k];
                output.Add(s / ratio);
            }
            else
            {
                if (i0 + 1 >= _mono.Count) break;
                double frac = _pos - i0;
                float a = _mono[i0], b = _mono[i0 + 1];
                output.Add((float)(a + (b - a) * frac));
            }
            _pos += step;
        }
        int consumed = Math.Min(_mono.Count, (int)Math.Floor(_pos));
        Pcm.RemoveHead(_mono, consumed);
        _pos -= consumed;
    }
}

internal sealed record SpeechSegment(float[] Samples, long StartQpc100ns, int DurationMs, string App = "");

/// <summary>
/// Energy pre-gate and segmenter on 16 kHz mono float: 20 ms frames, adaptive noise floor,
/// segments end after 600 ms of silence or at maxSegmentS (cut at the last quiet frame when possible).
/// Pure logic; the caller supplies timestamps.
/// </summary>
internal sealed class SpeechSegmenter
{
    public const int Rate = 16000;
    public const int FrameSamples = Rate / 50; // 20 ms
    private const int PreRollFrames = 10;      // 200 ms
    private const int EndSilenceFrames = 30;   // 600 ms
    private const int MinSpeechFrames = 15;    // 300 ms of speech in a segment

    private readonly Func<int> _maxSegmentS;
    private List<float> _pending = new();
    private readonly Queue<float[]> _preRoll = new();
    private List<float>? _seg;
    private long _segStart;
    private int _speechFrames;
    private int _silenceRun;
    private int _lastQuietFrameEnd = -1; // sample index in _seg after the last quiet frame
    private double _noiseDb = -60;
    private long _streamPos;             // samples consumed since start
    private long _streamStartQpc = long.MinValue;

    public double ThresholdDb { get; private set; } = -45;

    /// <summary>A segment is open (speech seen, end not reached yet).</summary>
    public bool Active => _seg is not null;

    /// <summary>QPC start (100 ns) of the open segment once it holds enough speech to be emitted, else null (a remainder
    /// of trailing silence after an emit stays open without speech).</summary>
    public long? OpenStartQpc => _seg is null || _speechFrames < MinSpeechFrames ? null : QpcAt(_segStart);

    /// <summary>The caller skipped packets (digital silence, no segment open): the next packet's time is taken as is.
    /// Without it the stream clock lagged the skipped time (up to the 0.5 s re-anchor limit), so segment times, and the
    /// guard verdicts looked up by them (A-H2), were early.</summary>
    public void Reanchor()
    {
        _streamStartQpc = long.MinValue;
        Pcm.Wipe(_pending);
    }

    public SpeechSegmenter(Func<int> maxSegmentS) { _maxSegmentS = maxSegmentS; }

    /// <summary>Feeds samples starting at qpc100ns. Emits finished segments.</summary>
    public void Push(ReadOnlySpan<float> samples, long qpc100ns, List<SpeechSegment> output)
    {
        if (_streamStartQpc == long.MinValue) _streamStartQpc = qpc100ns - _streamPos * 10_000_000L / Rate;
        // Re-anchor on large clock jumps (capture restarted, long silence without packets).
        long expected = _streamStartQpc + (_streamPos + _pending.Count) * 10_000_000L / Rate;
        if (Math.Abs(qpc100ns - expected) > 5_000_000)
        {
            Flush(output);
            Pcm.Wipe(_pending);
            _streamStartQpc = qpc100ns - _streamPos * 10_000_000L / Rate;
        }
        _pending = Pcm.Append(_pending, samples);
        int offset = 0;
        var frame = new float[FrameSamples];
        while (_pending.Count - offset >= FrameSamples)
        {
            _pending.CopyTo(offset, frame, 0, FrameSamples);
            offset += FrameSamples;
            ProcessFrame(frame, output);
            _streamPos += FrameSamples;
        }
        Pcm.RemoveHead(_pending, offset);
        Array.Clear(frame);
    }

    private long QpcAt(long streamSample) => _streamStartQpc + streamSample * 10_000_000L / Rate;

    private void ProcessFrame(float[] frame, List<SpeechSegment> output)
    {
        double sum = 0;
        foreach (var v in frame) sum += v * v;
        double rms = Math.Sqrt(sum / frame.Length);
        double db = 20 * Math.Log10(Math.Max(rms, 1e-9));
        bool speech = db > Math.Max(-50, _noiseDb + 12);
        // Noise floor follows quiet frames quickly and loud frames slowly.
        _noiseDb = db < _noiseDb ? _noiseDb * 0.9 + db * 0.1 : _noiseDb * 0.999 + db * 0.001;
        ThresholdDb = Math.Max(-50, _noiseDb + 12);

        if (_seg is null)
        {
            _preRoll.Enqueue((float[])frame.Clone());
            if (_preRoll.Count > PreRollFrames) Array.Clear(_preRoll.Dequeue());
            if (!speech) return;
            _seg = new List<float>(Rate * 4);
            _segStart = _streamPos - (_preRoll.Count - 1) * FrameSamples;
            foreach (var f in _preRoll) _seg = Pcm.Append(_seg, f);
            ClearPreRoll();
            _speechFrames = 1;
            _silenceRun = 0;
            _lastQuietFrameEnd = -1;
            return;
        }
        _seg = Pcm.Append(_seg, frame);
        if (speech) { _speechFrames++; _silenceRun = 0; }
        else { _silenceRun++; _lastQuietFrameEnd = _seg.Count; }

        if (_silenceRun >= EndSilenceFrames)
        {
            // Keep 200 ms of trailing silence.
            int keep = _seg.Count - (_silenceRun - PreRollFrames) * FrameSamples;
            Emit(Math.Max(FrameSamples, keep), output);
            return;
        }
        int max = Math.Max(5, _maxSegmentS()) * Rate;
        if (_seg.Count >= max)
        {
            // Cut at the last quiet frame in the final third, else hard cut.
            int cut = _lastQuietFrameEnd > max * 2 / 3 ? _lastQuietFrameEnd : _seg.Count;
            Emit(cut, output);
        }
    }

    private void Emit(int length, List<SpeechSegment> output)
    {
        var seg = _seg!;
        length = Math.Min(length, seg.Count);
        if (_speechFrames >= MinSpeechFrames)
        {
            var samples = CollectionsMarshal.AsSpan(seg)[..length].ToArray();   // no GetRange temporary list (CA-S2)
            output.Add(new SpeechSegment(samples, QpcAt(_segStart), length * 1000 / Rate));
        }
        // Remainder after a mid-speech cut starts a new segment immediately.
        if (length < seg.Count)
        {
            var rest = new List<float>(seg.Capacity);
            rest.AddRange(CollectionsMarshal.AsSpan(seg)[length..]);
            _segStart += length;
            _seg = rest;
            _speechFrames = 1;
            _silenceRun = 0;
            _lastQuietFrameEnd = -1;
        }
        else
        {
            _seg = null;
            ClearPreRoll();
        }
        // Zero our copy.
        Pcm.Wipe(seg);
    }

    private void ClearPreRoll()
    {
        while (_preRoll.Count > 0) Array.Clear(_preRoll.Dequeue());
    }

    /// <summary>Ends the current segment (stream stop, long gap).</summary>
    public void Flush(List<SpeechSegment> output)
    {
        if (_seg is not null) Emit(_seg.Count, output);
    }

    /// <summary>Panic: drop buffered audio.</summary>
    public void Clear()
    {
        if (_seg is not null) Pcm.Wipe(_seg);
        _seg = null;
        Pcm.Wipe(_pending);
        ClearPreRoll();
    }
}
