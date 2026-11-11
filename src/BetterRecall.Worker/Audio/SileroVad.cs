// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using BetterRecall.Worker.Inference;
using BetterRecall.Worker.Protocol;

namespace BetterRecall.Worker.Audio;

/// <summary>Silero VAD v5/v6 ONNX on the CPU (16 kHz): 512-sample windows with the previous 64 samples prepended,
/// recurrent state [2,1,128]. Segment rules follow silero-vad's get_speech_timestamps defaults.</summary>
public sealed class SileroVad : IDisposable
{
    public const int Window = 512, Context = 64, StateSize = 2 * 1 * 128;
    private readonly IInferenceSession _session;
    private readonly string _in, _state, _sr, _out, _stateOut;

    public float Threshold { get; init; } = 0.5f;
    public int MinSpeechMs { get; init; } = 250;
    public int MinSilenceMs { get; init; } = 100;
    public int SpeechPadMs { get; init; } = 30;

    public SileroVad(IInferenceBackend cpuBackend, string modelPath)
    {
        var cpu = cpuBackend.Devices.First(d => d.Kind == DeviceKind.Cpu);
        _session = cpuBackend.Load(modelPath, cpu, null);
        string Find(IReadOnlyList<TensorSpec> l, string key, int fallback)
            => l.FirstOrDefault(s => s.Name.Equals(key, StringComparison.OrdinalIgnoreCase))?.Name
               ?? (l.Count > fallback ? l[fallback].Name : throw new InvalidDataException("unexpected VAD graph"));
        _in = Find(_session.Inputs, "input", 0);
        _state = Find(_session.Inputs, "state", 1);
        _sr = Find(_session.Inputs, "sr", 2);
        _out = Find(_session.Outputs, "output", 0);
        _stateOut = Find(_session.Outputs, "stateN", 1);
    }

    /// <summary>Speech probability per 512-sample window (32 ms).</summary>
    public float[] Probabilities(ReadOnlySpan<float> pcm)
    {
        int windows = (pcm.Length + Window - 1) / Window;
        var probs = new float[windows];
        var state = new float[StateSize];
        var buf = new float[Context + Window];
        for (int w = 0; w < windows; w++)
        {
            // buf[0..64) keeps the tail of the previous window.
            int start = w * Window;
            int n = Math.Min(Window, pcm.Length - start);
            Array.Clear(buf, Context, Window);
            pcm.Slice(start, n).CopyTo(buf.AsSpan(Context));
            var outp = _session.Run([
                HostTensor.F(_in, (float[])buf.Clone(), 1, Context + Window),
                HostTensor.F(_state, state, 2, 1, 128),
                HostTensor.L(_sr, [16000L]),
            ]);
            if (!outp.TryGetValue(_out, out var p) || !outp.TryGetValue(_stateOut, out var s)) throw new DeviceFailedException("VAD output missing");
            probs[w] = p.Floats![0];
            state = s.Floats!;
            Array.Copy(buf, Window, buf, 0, Context);
        }
        return probs;
    }

    public IReadOnlyList<(int StartMs, int EndMs)> Segments(ReadOnlySpan<float> pcm) => Segment(Probabilities(pcm), pcm.Length);

    /// <summary>get_speech_timestamps: enter at Threshold, leave below Threshold-0.15 after MinSilenceMs, drop
    /// segments shorter than MinSpeechMs, pad by SpeechPadMs.</summary>
    internal IReadOnlyList<(int StartMs, int EndMs)> Segment(float[] probs, int totalSamples)
    {
        const int sr = 16000;
        float neg = Math.Max(Threshold - 0.15f, 0.01f);
        int minSpeech = sr * MinSpeechMs / 1000, minSilence = sr * MinSilenceMs / 1000, pad = sr * SpeechPadMs / 1000;
        var raw = new List<(int S, int E)>();
        bool triggered = false; int start = 0, tempEnd = 0;
        for (int i = 0; i < probs.Length; i++)
        {
            int cur = i * Window;
            if (probs[i] >= Threshold && tempEnd != 0) tempEnd = 0;
            if (probs[i] >= Threshold && !triggered) { triggered = true; start = cur; continue; }
            if (probs[i] < neg && triggered)
            {
                if (tempEnd == 0) tempEnd = cur;
                if (cur - tempEnd < minSilence) continue;
                if (tempEnd - start > minSpeech) raw.Add((start, tempEnd));
                triggered = false; tempEnd = 0;
            }
        }
        if (triggered && totalSamples - start > minSpeech) raw.Add((start, totalSamples));
        var segs = new List<(int, int)>(raw.Count);
        foreach (var (s, e) in raw)
            segs.Add((Math.Max(0, s - pad) * 1000 / sr, Math.Min(totalSamples, e + pad) * 1000 / sr));
        return segs;
    }

    public void Dispose() => _session.Dispose();
}
