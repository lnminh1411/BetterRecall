// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Threading.Channels;
using BetterRecall.Agent.Replay;
using BetterRecall.Agent.Util;
using BetterRecall.Agent.Worker;
using BetterRecall.Contracts;

namespace BetterRecall.Agent.Audio;

/// <summary>One process-loopback stream (48 kHz stereo 16-bit, own process tree excluded) shared by the replay and system transcripts.</summary>
internal sealed class LoopbackHub : IDisposable
{
    private readonly object _gate = new();
    private readonly List<PcmHandler> _subs = new();
    private WasapiCapture? _capture;

    public PcmFormat Format { get; } = new(48000, 2, false, 16);
    public bool Running => _capture is not null;

    public void Subscribe(PcmHandler h)
    {
        lock (_gate)
        {
            if (_subs.Contains(h)) return;
            _subs.Add(h);
            if (_capture is null)
            {
                try { _capture = WasapiCapture.Start("loopback", WAVEFORMATEX.Pcm16(48000, 2), Dispatch); }
                catch (Exception ex) { Log.Warn("loopback start", ex); _subs.Remove(h); throw; }
            }
        }
    }

    public void Unsubscribe(PcmHandler h)
    {
        WasapiCapture? stop = null;
        lock (_gate)
        {
            _subs.Remove(h);
            if (_subs.Count == 0) { stop = _capture; _capture = null; }
        }
        stop?.Dispose();
    }

    private void Dispatch(ReadOnlySpan<byte> data, int frames, long qpc, bool silent)
    {
        PcmHandler[] subs;
        lock (_gate) subs = _subs.ToArray();
        foreach (var s in subs)
        {
            try { s(data, frames, qpc, silent); }
            catch (Exception ex) { Log.Warn("audio subscriber", ex); }
        }
    }

    public void Dispose()
    {
        WasapiCapture? c;
        lock (_gate) { c = _capture; _capture = null; _subs.Clear(); }
        c?.Dispose();
    }
}

/// <summary>Desktop audio for the replay: AAC into a RAM ring with QPC timestamps.</summary>
internal sealed class AudioReplayEncoder : IDisposable
{
    private readonly AacEncoder _aac;
    private readonly LoopbackHub? _ownHub;
    private readonly LoopbackHub _hub;
    private readonly PcmHandler _handler;

    public PacketRing Ring { get; }
    public AudioRingSource Source { get; }
    public int Seconds { get; }
    public volatile bool Paused;
    public long FramesCaptured { get; private set; }
    public long Gaps => _aac.Gaps;

    public AudioReplayEncoder(LoopbackHub hub, int seconds, bool lockMemory)
    {
        _hub = hub;
        Seconds = seconds;
        // 128 kbps plus slack.
        Ring = new PacketRing(16_000L * (seconds + 4) * 5 / 4, TimeSpan.FromSeconds(seconds), lockMemory);
        _aac = new AacEncoder(Ring);
        Source = new AudioRingSource(Ring, AacEncoder.SampleRate, AacEncoder.Channels, AacEncoder.AudioSpecificConfig);
        _handler = OnPcm;
        hub.Subscribe(_handler);
    }

    private AudioReplayEncoder(LoopbackHub own, int seconds, bool lockMemory, bool owned) : this(own, seconds, lockMemory) { _ownHub = own; }

    /// <summary>Self-contained instance with its own loopback stream (dev self-test).</summary>
    public static AudioReplayEncoder Start(int seconds, bool lockMemory) => new(new LoopbackHub(), seconds, lockMemory, owned: true);

    private void OnPcm(ReadOnlySpan<byte> data, int frames, long qpc, bool silent)
    {
        if (Paused) return;
        FramesCaptured += frames;
        // Quiet packets keep the timeline inside a run (encoded as zeros); after 2 s of quiet the run ends,
        // so an idle desktop costs no AAC work.
        bool quiet = silent || IsNearSilent16(data);
        if (quiet) SilentPackets++;
        _aac.Push(data, qpc, quiet);
    }

    internal static bool IsAllZero(ReadOnlySpan<byte> data) => data.IndexOfAnyExcept((byte)0) < 0;

    /// <summary>Peak below about -54 dBFS on 16-bit PCM: dither and idle-device noise, not worth encoding.</summary>
    internal static bool IsNearSilent16(ReadOnlySpan<byte> data)
    {
        var samples = System.Runtime.InteropServices.MemoryMarshal.Cast<byte, short>(data);
        foreach (var v in samples)
            if (v > 64 || v < -64) return false;
        return true;
    }

    public long SilentPackets { get; private set; }

    public void Dispose()
    {
        _hub.Unsubscribe(_handler);
        _ownHub?.Dispose();
        _aac.Dispose();
        Ring.Dispose();
    }
}

/// <summary>
/// Speech transcripts for one source (system or mic): 16 kHz mono, energy gate and segmenter,
/// transcribe on the worker, store speech as an audio capture. Raw audio stays in RAM and is zeroed after use.
/// </summary>
internal sealed class TranscriptSource : IDisposable
{
    private readonly string _source;
    private readonly Func<Settings> _settings;
    private readonly Func<bool> _paused;
    private readonly Channel<SpeechSegment> _queue = Channel.CreateBounded<SpeechSegment>(new BoundedChannelOptions(8) { FullMode = BoundedChannelFullMode.DropOldest });
    private readonly SpeechSegmenter _segmenter;
    private readonly List<float> _buf = new();
    private readonly List<SpeechSegment> _out = new();
    private readonly object _gate = new();
    private Resampler? _resampler;
    private WasapiCapture? _mic;
    private readonly LoopbackHub? _hub;
    private readonly PcmHandler _handler;

    public long Segments { get; private set; }

    public TranscriptSource(string source, LoopbackHub? hub, Func<Settings> settings, Func<bool> paused)
    {
        _source = source;
        _hub = hub;
        _settings = settings;
        _paused = paused;
        _segmenter = new SpeechSegmenter(() => settings().Audio.MaxSegmentS);
        _handler = OnPcm;
        if (source == "system")
        {
            _resampler = new Resampler(hub!.Format);
            hub.Subscribe(_handler);
        }
        else
        {
            _mic = WasapiCapture.Start("mic", WAVEFORMATEX.Float32(16000, 1), _handler);
            _resampler = new Resampler(_mic.Format);
        }
    }

    public ChannelReader<SpeechSegment> Segments_ => _queue.Reader;

    private void OnPcm(ReadOnlySpan<byte> data, int frames, long qpc, bool silent)
    {
        if (_paused()) return;
        bool quiet = silent || AudioReplayEncoder.IsAllZero(data);
        lock (_gate)
        {
            // Digital silence with no segment open: nothing to do (keeps an idle desktop cheap).
            if (quiet && !_segmenter.Active) { _resampler!.Reset(); return; }
            _buf.Clear();
            _resampler!.Process(data, frames, quiet, _buf);
            _out.Clear();
            _segmenter.Push(System.Runtime.InteropServices.CollectionsMarshal.AsSpan(_buf), qpc, _out);
            foreach (var s in _out) { Segments++; _queue.Writer.TryWrite(s); }
            _buf.Clear();
        }
    }

    public string Source => _source;

    public void Clear()
    {
        lock (_gate)
        {
            _segmenter.Clear();
            _buf.Clear();
            while (_queue.Reader.TryRead(out var s)) Array.Clear(s.Samples);
        }
    }

    public void Dispose()
    {
        if (_hub is not null) _hub.Unsubscribe(_handler);
        _mic?.Dispose();
        _queue.Writer.TryComplete();
        Clear();
    }
}

/// <summary>Sends speech segments to the worker and stores transcripts.</summary>
internal sealed class Transcriber
{
    private readonly WorkerClient _worker;
    private readonly Func<IStore?> _store;
    private readonly Func<Settings> _settings;
    private readonly Func<string> _foregroundApp;
    private readonly Action _stored;

    public Transcriber(WorkerClient worker, Func<IStore?> store, Func<Settings> settings, Func<string> foregroundApp, Action stored)
    {
        _worker = worker;
        _store = store;
        _settings = settings;
        _foregroundApp = foregroundApp;
        _stored = stored;
    }

    public async Task RunAsync(TranscriptSource src, CancellationToken ct)
    {
        await foreach (var seg in src.Segments_.ReadAllAsync(ct).ConfigureAwait(false))
        {
            try
            {
                var s = _settings();
                var app = _foregroundApp();
                var r = await _worker.TranscribeAsync(seg.Samples, src.Source, s.Audio.Language, ct).ConfigureAwait(false);
                if (!r.Speech || string.IsNullOrWhiteSpace(r.Text)) continue;
                var store = _store();
                if (store is null) continue;
                var ts = QpcToWall(seg.StartQpc100ns);
                store.AddAudioCapture(new NewAudioCapture(ts, src.Source, seg.DurationMs, r.Text.Trim(), r.Language, app));
                _stored();
            }
            catch (OperationCanceledException) when (ct.IsCancellationRequested) { break; }
            catch (PurgedWindowException) { Log.Debug($"transcribe {src.Source}: segment inside a purged window dropped"); }
            catch (Exception ex) { Log.Warn($"transcribe {src.Source}", ex); }
            finally { Array.Clear(seg.Samples); }
        }
    }

    /// <summary>Maps a QPC time (100 ns) to wall clock using the current offset.</summary>
    public static DateTimeOffset QpcToWall(long qpc100ns)
    {
        long now = Capture.OutputCapture.Qpc100ns(System.Diagnostics.Stopwatch.GetTimestamp());
        return DateTimeOffset.UtcNow - TimeSpan.FromTicks(now - qpc100ns);
    }
}
