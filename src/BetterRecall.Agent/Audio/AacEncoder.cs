// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using BetterRecall.Agent.Native;
using BetterRecall.Agent.Replay;
using BetterRecall.Agent.Util;
using Vortice.MediaFoundation;

namespace BetterRecall.Agent.Audio;

/// <summary>
/// AAC-LC encoder (Media Foundation software MFT, synchronous) from 48 kHz stereo 16-bit PCM into a packet ring.
/// Timing comes from <see cref="AudioTimeline"/>: silence and gaps up to 2 s are encoded as zeros,
/// longer silence ends the run (drain), and packet times are run start + 1024 samples per AAC frame.
/// </summary>
internal sealed unsafe class AacEncoder : IDisposable
{
    private static readonly Guid CLSID_AACMFTEncoder = new("93AF0C51-2275-45d2-A35B-F2BA21CAED00");
    private static readonly Guid IID_IMFTransform = new("bf94c121-5b05-4e6f-8000-ba598961414d");
    private static readonly int MF_E_TRANSFORM_NEED_MORE_INPUT = unchecked((int)0xC00D6D72);

    public const int SampleRate = 48000, Channels = 2;
    private readonly IMFTransform _mft;
    private readonly PacketRing _ring;
    private readonly int _outSize;
    private readonly object _gate = new();
    private const int AacFrame = 1024;
    private readonly AudioTimeline _timeline = new(SampleRate);
    private long _outFrames;
    private long _inPts;
    private byte[] _silence = new byte[SampleRate / 50 * 4];

    public long Gaps { get; private set; }
    public long FramesIn { get; private set; }
    public long Runs => _timeline.Runs;

    /// <summary>AudioSpecificConfig for AAC-LC 48 kHz stereo.</summary>
    public static readonly byte[] AudioSpecificConfig = [0x11, 0x90];

    public AacEncoder(PacketRing ring)
    {
        MediaFoundationRuntime.EnsureStarted();
        _ring = ring;
        nint p = Ole.Create(CLSID_AACMFTEncoder, IID_IMFTransform);
        _mft = new IMFTransform(p);

        using var inType = MediaFactory.MFCreateMediaType();
        inType.Set(MediaTypeAttributeKeys.MajorType, MediaTypeGuids.Audio);
        inType.Set(MediaTypeAttributeKeys.Subtype, AudioFormatGuids.Pcm);
        inType.Set(MediaTypeAttributeKeys.AudioBitsPerSample, 16u);
        inType.Set(MediaTypeAttributeKeys.AudioSamplesPerSecond, (uint)SampleRate);
        inType.Set(MediaTypeAttributeKeys.AudioNumChannels, (uint)Channels);
        inType.Set(MediaTypeAttributeKeys.AudioBlockAlignment, (uint)(Channels * 2));
        inType.Set(MediaTypeAttributeKeys.AudioAvgBytesPerSecond, (uint)(SampleRate * Channels * 2));
        using var outType = MediaFactory.MFCreateMediaType();
        outType.Set(MediaTypeAttributeKeys.MajorType, MediaTypeGuids.Audio);
        outType.Set(MediaTypeAttributeKeys.Subtype, AudioFormatGuids.Aac);
        outType.Set(MediaTypeAttributeKeys.AudioBitsPerSample, 16u);
        outType.Set(MediaTypeAttributeKeys.AudioSamplesPerSecond, (uint)SampleRate);
        outType.Set(MediaTypeAttributeKeys.AudioNumChannels, (uint)Channels);
        outType.Set(MediaTypeAttributeKeys.AudioAvgBytesPerSecond, 16000u); // 128 kbps
        outType.Set(MediaTypeAttributeKeys.AacPayloadType, 0u);              // raw AAC
        outType.Set(MediaTypeAttributeKeys.AacAudioProfileLevelIndication, 0x29u);
        _mft.SetInputType(0, inType, 0);
        _mft.SetOutputType(0, outType, 0);
        _outSize = Math.Max(8192, _mft.GetOutputStreamInfo(0).Size);
        _mft.ProcessMessage(TMessageType.MessageNotifyBeginStreaming, 0);
        _mft.ProcessMessage(TMessageType.MessageNotifyStartOfStream, 0);
    }

    /// <summary>pcm: interleaved 16-bit stereo at 48 kHz. qpc: time of the first frame, 100 ns QPC units.
    /// silent: the packet carries no audible sound (it is encoded as zeros while a run lasts).</summary>
    public void Push(ReadOnlySpan<byte> pcm, long qpc, bool silent)
    {
        if (pcm.IsEmpty) return;
        lock (_gate)
        {
            var step = _timeline.Plan(qpc, pcm.Length / 4, silent);
            if (step.EndRun) EndRun();
            if (step.StartRun) _outFrames = 0;
            _timeline.Commit(step, qpc);
            if (!_timeline.InRun) return;
            if (step.FillFrames > 0) { FeedSilence(step.FillFrames); Gaps++; }
            if (step.FeedFrames > 0)
            {
                if (step.FeedSilent) FeedSilence(step.FeedFrames);
                else Feed(pcm.Slice(step.SkipFrames * 4, step.FeedFrames * 4));
            }
        }
    }

    private void FeedSilence(long frames)
    {
        while (frames > 0)
        {
            int n = (int)Math.Min(frames, _silence.Length / 4);
            Feed(_silence.AsSpan(0, n * 4));
            frames -= n;
        }
    }

    /// <summary>Drains what the encoder still holds into the ring, stamped with the ending run's times.</summary>
    private void EndRun()
    {
        if (!_timeline.InRun) return;
        _mft.ProcessMessage(TMessageType.MessageCommandDrain, 0);
        Drain();
        _mft.ProcessMessage(TMessageType.MessageCommandFlush, 0);
    }

    private void Feed(ReadOnlySpan<byte> pcm)
    {
        int frames = pcm.Length / 4;
        using (var buffer = MediaFactory.MFCreateMemoryBuffer(pcm.Length))
        {
            buffer.Lock(out nint ptr, out _, out _);
            pcm.CopyTo(new Span<byte>((void*)ptr, pcm.Length));
            buffer.Unlock();
            buffer.CurrentLength = pcm.Length;
            using var sample = MediaFactory.MFCreateSample();
            sample.AddBuffer(buffer);
            sample.SampleTime = _inPts;
            sample.SampleDuration = frames * 10_000_000L / SampleRate;
            _mft.ProcessInput(0, sample, 0);
            _inPts += sample.SampleDuration;
        }
        FramesIn += frames;
        Drain();
    }

    private void Drain()
    {
        while (true)
        {
            using var outBuf = MediaFactory.MFCreateMemoryBuffer(_outSize);
            using var outSample = MediaFactory.MFCreateSample();
            outSample.AddBuffer(outBuf);
            var odb = new OutputDataBuffer { StreamID = 0, Sample = outSample };
            var r = _mft.ProcessOutput(ProcessOutputFlags.None, 1, ref odb, out _);
            odb.Events?.Dispose();
            if (r.Code == MF_E_TRANSFORM_NEED_MORE_INPUT) return;
            if (r.Failure) { Log.Debug($"aac ProcessOutput 0x{r.Code:X8}"); return; }
            // Packet times come from the run's sample clock, not the MFT: one AAC frame is 1024 samples.
            long t = _timeline.RunStart + _outFrames * AacFrame * 10_000_000L / SampleRate;
            _outFrames++;
            outBuf.Lock(out nint ptr, out _, out int len);
            try
            {
                if (len > 0) _ring.Write(new ReadOnlySpan<byte>((void*)ptr, len), t, key: true);
            }
            finally
            {
                new Span<byte>((void*)ptr, len).Clear();
                outBuf.Unlock();
            }
        }
    }

    public void Dispose()
    {
        lock (_gate)
        {
            try { _mft.ProcessMessage(TMessageType.MessageNotifyEndOfStream, 0); } catch { }
            _mft.Dispose();
        }
    }
}
