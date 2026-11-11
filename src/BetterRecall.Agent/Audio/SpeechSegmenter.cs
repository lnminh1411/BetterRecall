// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
namespace BetterRecall.Agent.Audio;

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

    public void Reset() { _pos = 0; _mono.Clear(); }

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
            _mono.Clear();
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
        _mono.RemoveRange(0, consumed);
        _pos -= consumed;
    }
}

internal sealed record SpeechSegment(float[] Samples, long StartQpc100ns, int DurationMs);

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
    private readonly List<float> _pending = new();
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
            _pending.Clear();
            _streamStartQpc = qpc100ns - _streamPos * 10_000_000L / Rate;
        }
        foreach (var s in samples) _pending.Add(s);
        int offset = 0;
        var frame = new float[FrameSamples];
        while (_pending.Count - offset >= FrameSamples)
        {
            _pending.CopyTo(offset, frame, 0, FrameSamples);
            offset += FrameSamples;
            ProcessFrame(frame, output);
            _streamPos += FrameSamples;
        }
        _pending.RemoveRange(0, offset);
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
            if (_preRoll.Count > PreRollFrames) _preRoll.Dequeue();
            if (!speech) return;
            _seg = new List<float>(Rate * 4);
            _segStart = _streamPos - (_preRoll.Count - 1) * FrameSamples;
            foreach (var f in _preRoll) _seg.AddRange(f);
            _preRoll.Clear();
            _speechFrames = 1;
            _silenceRun = 0;
            _lastQuietFrameEnd = -1;
            return;
        }
        _seg.AddRange(frame);
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
            var samples = seg.GetRange(0, length).ToArray();
            output.Add(new SpeechSegment(samples, QpcAt(_segStart), length * 1000 / Rate));
        }
        // Remainder after a mid-speech cut starts a new segment immediately.
        if (length < seg.Count)
        {
            var rest = seg.GetRange(length, seg.Count - length);
            _segStart += length;
            _seg = rest;
            _speechFrames = 1;
            _silenceRun = 0;
            _lastQuietFrameEnd = -1;
        }
        else
        {
            _seg = null;
            _preRoll.Clear();
        }
        // Zero our copy.
        seg.Clear();
    }

    /// <summary>Ends the current segment (stream stop, long gap).</summary>
    public void Flush(List<SpeechSegment> output)
    {
        if (_seg is not null) Emit(_seg.Count, output);
    }

    /// <summary>Panic: drop buffered audio.</summary>
    public void Clear()
    {
        _seg?.Clear();
        _seg = null;
        _pending.Clear();
        _preRoll.Clear();
    }
}
