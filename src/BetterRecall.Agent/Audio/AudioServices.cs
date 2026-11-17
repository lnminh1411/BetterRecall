// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Threading.Channels;
using BetterRecall.Agent.Guardrails;
using BetterRecall.Agent.Privacy;
using BetterRecall.Agent.Replay;
using BetterRecall.Agent.Util;
using BetterRecall.Agent.Worker;
using BetterRecall.Contracts;

namespace BetterRecall.Agent.Audio;

/// <summary>A running capture stream (WASAPI, or a fake in tests).</summary>
internal interface ICaptureStream : IDisposable
{
    /// <summary>The capture loop has exited: no more packets will come.</summary>
    bool Ended { get; }
}

/// <summary>One process-loopback stream (48 kHz stereo 16-bit, own process tree excluded) shared by the replay and system transcripts.
/// A stream whose thread ended (audio service restart, endpoint invalidated, loop error) is restarted with backoff while
/// anyone is subscribed (A13); subscribers stay attached across the restart.</summary>
internal sealed class LoopbackHub : IDisposable
{
    private readonly object _gate = new();
    private readonly List<PcmHandler> _subs = new();
    private readonly Func<PcmHandler, ICaptureStream> _open;
    private readonly RetryBackoff _restart = new(1000, 60_000);
    private ICaptureStream? _capture;
    private long _upSinceMs;

    public LoopbackHub() : this(h => WasapiCapture.Start("loopback", WAVEFORMATEX.Pcm16(48000, 2), h)) { }

    internal LoopbackHub(Func<PcmHandler, ICaptureStream> open) => _open = open;

    public PcmFormat Format { get; } = new(48000, 2, false, 16);
    public bool Running { get { lock (_gate) return _capture is { Ended: false }; } }
    /// <summary>Restarts after a lost stream (tests and diagnostics).</summary>
    public int Restarts { get; private set; }

    public void Subscribe(PcmHandler h)
    {
        lock (_gate)
        {
            if (_subs.Contains(h)) return;
            _subs.Add(h);
            if (_capture is { Ended: true }) { _capture.Dispose(); _capture = null; }
            if (_capture is null)
            {
                try { _capture = _open(Dispatch); _restart.Succeeded(); }
                catch (Exception ex) { Log.Warn("loopback start", ex); _subs.Remove(h); throw; }
            }
        }
    }

    /// <summary>Called from the status tick: when the stream ended while subscribers remain, open a new one once the
    /// backoff allows. Returns true when a new stream is running.</summary>
    public bool CheckHealth(long nowMs)
    {
        ICaptureStream? dead = null;
        bool restarted = false;
        // A restart can block for seconds (device activation): a status tick that finds it busy skips its turn.
        if (!Monitor.TryEnter(_gate)) return false;
        try
        {
            if (_subs.Count == 0) return false;
            if (_capture is { Ended: true })
            {
                dead = _capture;
                _capture = null;
                Log.Warn("loopback stream ended; restarting");
                _restart.Failed(nowMs);   // the first restart waits one backoff step
            }
            if (_capture is null && _restart.Due(nowMs))
            {
                try
                {
                    _capture = _open(Dispatch);
                    Restarts++;
                    restarted = true;
                    _upSinceMs = nowMs;
                    Log.Info("loopback stream restarted");
                }
                catch (Exception ex) { Log.Warn($"loopback restart (next in {_restart.Failed(nowMs) / 1000} s)", ex); }
            }
            else if (_capture is { Ended: false } && _restart.Failures > 0 && nowMs - _upSinceMs >= 60_000)
                _restart.Succeeded();   // a minute up: the next loss starts at 1 s again
        }
        finally { Monitor.Exit(_gate); }
        dead?.Dispose();
        return restarted;
    }

    public void Unsubscribe(PcmHandler h)
    {
        ICaptureStream? stop = null;
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
        ICaptureStream? c;
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

    /// <summary>A-H2 (D1): true while the guard keeps audio out (a denied, private or protected window is in front, or
    /// the foreground was not scanned yet). Those packets go into the ring as silence. Null: never muted.</summary>
    private readonly Func<bool>? _muted;
    /// <summary>Packets replaced by silence because the guard muted them (tests, diagnostics).</summary>
    public long MutedPackets { get; private set; }

    public AudioReplayEncoder(LoopbackHub hub, int seconds, bool lockMemory, Func<bool>? muted = null)
    {
        _hub = hub;
        _muted = muted;
        Seconds = seconds;
        // 128 kbps plus slack.
        Ring = new PacketRing(16_000L * (seconds + 4) * 5 / 4, TimeSpan.FromSeconds(seconds), lockMemory);
        try
        {
            _aac = new AacEncoder(Ring);
            Source = new AudioRingSource(Ring, AacEncoder.SampleRate, AacEncoder.Channels, AacEncoder.AudioSpecificConfig);
            _handler = OnPcm;
            hub.Subscribe(_handler);
        }
        catch
        {
            // Loopback unavailable (audio service down, endpoint gone): free the MFT and the ring, which leaked once per
            // retry before (A15).
            _aac?.Dispose();
            Ring.Dispose();
            throw;
        }
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
        bool muted = _muted?.Invoke() == true;
        if (muted) MutedPackets++;
        bool quiet = silent || muted || IsNearSilent16(data);
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
    // A segment DropOldest evicts (worker slow) is zeroed like a transcribed one (A16).
    private readonly Channel<SpeechSegment> _queue = Channel.CreateBounded<SpeechSegment>(
        new BoundedChannelOptions(8) { FullMode = BoundedChannelFullMode.DropOldest }, s => Array.Clear(s.Samples));
    private readonly SpeechSegmenter _segmenter;
    private readonly List<float> _buf = new();
    private readonly List<SpeechSegment> _out = new();
    private readonly object _gate = new();
    private Resampler? _resampler;
    private WasapiCapture? _mic;

    /// <summary>The microphone capture ended on its own (device unplugged or changed).</summary>
    public bool MicLost => _mic?.Lost == true;
    private readonly LoopbackHub? _hub;
    private readonly PcmHandler _handler;

    public long Segments { get; private set; }

    // A-H2: the guard's audio verdict per packet, kept as a timeline so each segment is judged by when it was spoken.
    private readonly Func<AudioVerdict>? _guard;
    private readonly AudioGuardTimeline _guardTimeline = new();
    private int _clearGen;

    /// <summary>Bumped by Clear (panic, purge reaching the replay window): segments taken before it are dropped, also
    /// ones the transcriber holds for a retry or for redaction across segments (A-M5, A-M7).</summary>
    public int ClearGen => Volatile.Read(ref _clearGen);

    public TranscriptSource(string source, LoopbackHub? hub, Func<Settings> settings, Func<bool> paused, Func<AudioVerdict>? guard = null)
    {
        _source = source;
        _hub = hub;
        _settings = settings;
        _paused = paused;
        _guard = guard;
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
        var verdict = _guard?.Invoke() ?? AudioVerdict.Allowed;
        bool quiet = silent || AudioReplayEncoder.IsAllZero(data);
        lock (_gate)
        {
            _guardTimeline.Note(qpc, verdict);
            // Digital silence with no segment open: nothing to do (keeps an idle desktop cheap).
            if (quiet && !_segmenter.Active) { _resampler!.Reset(); _segmenter.Reanchor(); return; }
            Pcm.Wipe(_buf);
            _resampler!.Process(data, frames, quiet, _buf);
            _out.Clear();
            _segmenter.Push(System.Runtime.InteropServices.CollectionsMarshal.AsSpan(_buf), qpc, _out);
            foreach (var s in _out) { Segments++; _queue.Writer.TryWrite(s); }
            Pcm.Wipe(_buf);
        }
    }

    public string Source => _source;

    /// <summary>Guard verdict for a segment from the verdicts noted while it was spoken (A-H2): Blocked when a denied,
    /// private or protected window was in front at any point, Unknown while a foreground change in it is not scanned yet,
    /// with the app in front for most of it as the label.</summary>
    public AudioVerdict GuardFor(SpeechSegment s)
    {
        lock (_gate) return _guardTimeline.Check(s.StartQpc100ns, s.StartQpc100ns + s.DurationMs * 10_000L);
    }

    /// <summary>QPC start of the segment being spoken now, else null: the transcriber keeps holding a transcript the
    /// open segment will join (A-M7).</summary>
    public long? OpenSegmentStart { get { lock (_gate) return _segmenter.OpenStartQpc; } }

    /// <summary>Panic or a purge reaching the replay window: drops buffered and queued audio, and what the transcriber
    /// still holds of this source.</summary>
    public void Clear()
    {
        Interlocked.Increment(ref _clearGen);
        Wipe();
    }

    private void Wipe()
    {
        lock (_gate)
        {
            _segmenter.Clear();
            Pcm.Wipe(_buf);
            _resampler?.Reset();
            while (_queue.Reader.TryRead(out var s)) Array.Clear(s.Samples);
        }
    }

    public void Dispose()
    {
        if (_hub is not null) _hub.Unsubscribe(_handler);
        _mic?.Dispose();
        _queue.Writer.TryComplete();
        Wipe();   // audio turned off: what the transcriber already holds is still stored
    }
}

/// <summary>Guard verdicts over time for one audio source (A-H2): one entry per change, stamped with the packet's QPC
/// time, so a segment is judged by what was in front while it was spoken, not when the worker gets to it. Unknown (a
/// foreground change the guard has not scanned yet) takes the next known verdict. Not thread-safe: TranscriptSource
/// calls it under its lock.</summary>
internal sealed class AudioGuardTimeline
{
    public const long KeepFor100ns = 10L * 60 * 10_000_000;   // ten minutes: queued and retried segments stay covered
    public const int MaxEntries = 4096;
    private const long StartSlack100ns = 5_000_000;           // segment start rounding against the first packet
    private readonly List<(long At, AudioVerdict V)> _entries = new();

    public int Count => _entries.Count;

    public void Note(long qpc100ns, AudioVerdict v)
    {
        if (_entries.Count > 0 && _entries[^1].V == v) return;
        _entries.Add((qpc100ns, v));
        // Entry 0 stays as the state before the cut: drop only entries that a newer one already supersedes.
        int drop = 0;
        while (drop + 1 < _entries.Count && (qpc100ns - _entries[drop + 1].At > KeepFor100ns || _entries.Count - drop > MaxEntries)) drop++;
        if (drop > 0) _entries.RemoveRange(0, drop);
    }

    /// <summary>Blocked when any verdict in [start, end] blocks, Unknown when one is still unresolved, else Allowed;
    /// the app is the one in front for most of the span (a segment opens on the silence before the speech). A segment
    /// older than the timeline is Blocked (nothing says what was in front).</summary>
    public AudioVerdict Check(long start, long end)
    {
        if (_entries.Count == 0 || start < _entries[0].At - StartSlack100ns) return new(AudioGate.Blocked, "");
        int i = 0;
        while (i + 1 < _entries.Count && _entries[i + 1].At <= start) i++;
        var gate = AudioGate.Allowed;
        var time = new Dictionary<string, long>(StringComparer.Ordinal);
        string app = "";
        long best = -1;
        for (int j = i; j < _entries.Count && (j == i || _entries[j].At <= end); j++)
        {
            var v = Resolve(j);
            if (v.Gate == AudioGate.Blocked) return new(AudioGate.Blocked, v.App);
            if (v.Gate == AudioGate.Unknown) gate = AudioGate.Unknown;
            long to = j + 1 < _entries.Count ? Math.Min(end, _entries[j + 1].At) : end;
            long t = time[v.App] = time.GetValueOrDefault(v.App) + Math.Max(0, to - Math.Max(start, _entries[j].At));
            if (t > best) { best = t; app = v.App; }
        }
        return new(gate, app);
    }

    private AudioVerdict Resolve(int j)
    {
        for (int k = j; k < _entries.Count; k++)
            if (_entries[k].V.Gate != AudioGate.Unknown) return _entries[k].V;
        return _entries[j].V;
    }
}

/// <summary>
/// Sends speech segments to the worker and stores transcripts. A segment spoken while the guard blocked audio is never
/// transcribed (A-H2, D1). A worker failure that may pass (pipe closed, restart backoff, recycle, start timeout) keeps
/// the segment and retries it with backoff instead of dropping it (A-M5). Transcripts go through the privacy.redact*
/// text detectors before they are stored; each one is held a few seconds so a number split across two segments is
/// still found in the two texts joined (A-M7).
/// </summary>
internal sealed class Transcriber
{
    /// <summary>Tries per segment; the delays between them add up to about two minutes (worker backoff tops at 60 s).</summary>
    public const int MaxAttempts = 7;
    /// <summary>A transcript waits this long for the next segment of its source before it is stored alone (longer while
    /// that segment is still being spoken).</summary>
    public const int HoldMs = 4000;
    /// <summary>Two segments are checked together when the second starts at most this long after the first ended.</summary>
    public const long JoinGap100ns = 30_000_000;
    /// <summary>Longest hold while a joining segment is still being spoken (audio.maxSegmentS cuts it before).</summary>
    public const int MaxHoldMs = 120_000;

    private readonly WorkerClient _worker;
    private readonly Func<IStore?> _store;
    private readonly Func<Settings> _settings;
    private readonly Action _stored;

    /// <summary>Wait before try n+1 after try n failed (tests shorten it).</summary>
    internal Func<int, TimeSpan> RetryDelay { get; init; } = n => TimeSpan.FromSeconds(Math.Min(60, 2 << Math.Min(n - 1, 5)));
    /// <summary>Longest wait for the guard to resolve a foreground change inside a segment (then it counts as blocked).</summary>
    internal TimeSpan GuardWait { get; init; } = TimeSpan.FromSeconds(2);
    /// <summary>HoldMs (tests shorten it).</summary>
    internal int HoldFor { get; init; } = HoldMs;
    public long Retried, GaveUp, Blocked;

    public Transcriber(WorkerClient worker, Func<IStore?> store, Func<Settings> settings, Action stored)
    {
        _worker = worker;
        _store = store;
        _settings = settings;
        _stored = stored;
    }

    /// <summary>A transcript waiting for the next segment of its source before it is redacted and stored.</summary>
    private sealed class Held(DateTimeOffset ts, int durationMs, string raw, string language, string app, int gen, long endQpc)
    {
        public DateTimeOffset Ts { get; } = ts;
        public int DurationMs { get; } = durationMs;
        public string Raw { get; } = raw;
        public string Language { get; } = language;
        public string App { get; } = app;
        public int Gen { get; } = gen;
        public long EndQpc { get; } = endQpc;
        public long DueAt { get; set; }
        public long HeldAt { get; } = Environment.TickCount64;
        /// <summary>Spans found only with the neighbouring segment (a number split across both).</summary>
        public List<(int S, int E)> Forced { get; } = new();
    }

    public async Task RunAsync(TranscriptSource src, CancellationToken ct)
    {
        var reader = src.Segments_;
        Held? held = null;
        Task<bool>? next = null;
        try
        {
            while (true)
            {
                next ??= reader.WaitToReadAsync(ct).AsTask();
                if (held is not null && !next.IsCompleted)
                {
                    long wait = held.DueAt - Environment.TickCount64;
                    bool arrived = false;
                    if (wait > 0)
                    {
                        using var delay = CancellationTokenSource.CreateLinkedTokenSource(ct);
                        arrived = await Task.WhenAny(next, Task.Delay(TimeSpan.FromMilliseconds(wait), delay.Token)).ConfigureAwait(false) == next;
                        delay.Cancel();
                    }
                    ct.ThrowIfCancellationRequested();
                    if (!arrived)
                    {
                        // The next segment is still being spoken right after this one (a number read out across a cut):
                        // keep holding until it is transcribed.
                        long now = Environment.TickCount64;
                        if (src.OpenSegmentStart is { } open && open - held.EndQpc <= JoinGap100ns && now - held.HeldAt < MaxHoldMs)
                        {
                            held.DueAt = now + 500;
                            continue;
                        }
                        StoreHeld(src, held);
                        held = null;
                        continue;
                    }
                }
                bool more = await next.ConfigureAwait(false);
                next = null;
                if (!more) break;
                if (!reader.TryRead(out var seg)) continue;
                held = await ProcessAsync(src, seg, held, ct).ConfigureAwait(false);
            }
            // The source closed (audio turned off): nothing will join the held transcript any more.
            if (held is not null) StoreHeld(src, held);
        }
        catch (OperationCanceledException) when (ct.IsCancellationRequested) { }
    }

    /// <summary>Transcribes one segment; returns the transcript now held (this one, or the previous when this one gave
    /// nothing).</summary>
    private async Task<Held?> ProcessAsync(TranscriptSource src, SpeechSegment seg, Held? held, CancellationToken ct)
    {
        int gen = src.ClearGen;
        try
        {
            var verdict = await GuardAsync(src, seg, ct).ConfigureAwait(false);
            if (verdict.Gate != AudioGate.Allowed)
            {
                Interlocked.Increment(ref Blocked);
                Log.Debug($"transcribe {src.Source}: segment spoken while the guard kept audio out ({verdict.Gate}), dropped");
                return held;
            }
            var r = await TranscribeAsync(src, seg, gen, ct).ConfigureAwait(false);
            if (r is null || !r.Speech || string.IsNullOrWhiteSpace(r.Text)) return held;
            long end = seg.StartQpc100ns + seg.DurationMs * 10_000L;
            var cur = new Held(QpcToWall(seg.StartQpc100ns), seg.DurationMs, r.Text.Trim(), r.Language, verdict.App, gen, end);
            if (held is not null && held.Gen == gen && seg.StartQpc100ns - held.EndQpc <= JoinGap100ns)
            {
                var (a, b) = BoundarySpans(held.Raw, cur.Raw, RedactionOptions.From(_settings().Privacy));
                held.Forced.AddRange(a);
                cur.Forced.AddRange(b);
            }
            if (held is not null) StoreHeld(src, held);
            cur.DueAt = Environment.TickCount64 + HoldFor;
            return cur;
        }
        catch (OperationCanceledException) when (ct.IsCancellationRequested) { throw; }
        catch (Exception ex)
        {
            Log.Warn($"transcribe {src.Source}", ex);
            return held;
        }
        finally { Array.Clear(seg.Samples); }
    }

    /// <summary>The segment's guard verdict; a foreground change in it that the guard has not scanned yet is waited for
    /// briefly, then counts as blocked.</summary>
    private async Task<AudioVerdict> GuardAsync(TranscriptSource src, SpeechSegment seg, CancellationToken ct)
    {
        var v = src.GuardFor(seg);
        long until = Environment.TickCount64 + (long)GuardWait.TotalMilliseconds;
        while (v.Gate == AudioGate.Unknown && Environment.TickCount64 < until)
        {
            await Task.Delay(100, ct).ConfigureAwait(false);
            v = src.GuardFor(seg);
        }
        return v;
    }

    /// <summary>The worker's transcript, or null when the segment was dropped by a panic or purge while it waited.
    /// Passing worker failures are retried; an error reply (model not installed, bad input) or the last failure throws.</summary>
    private async Task<TranscribeResult?> TranscribeAsync(TranscriptSource src, SpeechSegment seg, int gen, CancellationToken ct)
    {
        for (int attempt = 1; ; attempt++)
        {
            try { return await _worker.TranscribeAsync(seg.Samples, src.Source, _settings().Audio.Language, ct).ConfigureAwait(false); }
            catch (WorkerException ex) when (ex is not WorkerReplyException && attempt < MaxAttempts)
            {
                Interlocked.Increment(ref Retried);
                var delay = RetryDelay(attempt);
                Log.Debug($"transcribe {src.Source}: {ex.Message} Segment kept, try {attempt + 1} in {delay.TotalSeconds:0.#} s");
                await Task.Delay(delay, ct).ConfigureAwait(false);
                if (src.ClearGen != gen) { Log.Debug($"transcribe {src.Source}: segment cleared while it waited for the worker"); return null; }
            }
            catch (WorkerException ex) when (ex is not WorkerReplyException)
            {
                Interlocked.Increment(ref GaveUp);
                throw new WorkerException($"segment dropped after {MaxAttempts} tries: {ex.Message}");
            }
        }
    }

    /// <summary>Redacts a held transcript with the current settings and stores it, unless a panic or purge cleared its
    /// source since it was spoken.</summary>
    private void StoreHeld(TranscriptSource src, Held h)
    {
        if (src.ClearGen != h.Gen) { Log.Debug($"transcribe {src.Source}: held transcript cleared"); return; }
        try
        {
            var store = _store();
            if (store is null) return;
            var text = Redact(h.Raw, RedactionOptions.From(_settings().Privacy), h.Forced);
            if (text.Length == 0) return;
            store.AddAudioCapture(new NewAudioCapture(h.Ts, src.Source, h.DurationMs, text, h.Language, h.App));
            _stored();
        }
        catch (PurgedWindowException) { Log.Debug($"transcribe {src.Source}: segment inside a purged window dropped"); }
        catch (Exception ex) { Log.Warn($"transcribe {src.Source}: store", ex); }
    }

    /// <summary>Transcript text with the text detectors' spans and the forced spans left out. Pure.</summary>
    internal static string Redact(string raw, RedactionOptions o, IEnumerable<(int S, int E)>? forced = null)
    {
        var spans = Redactor.TranscriptSpans(raw, o);
        if (forced is not null) spans = Redactor.Merge([.. spans, .. forced], raw.Length);
        return Redactor.Remove(raw, spans);
    }

    /// <summary>Spans of two neighbouring transcripts found in their joined text, split back onto each (A-M7): Whisper
    /// cut "4111 1111 | 1111 1111" into two segments, so neither held a whole card number. Pure.</summary>
    internal static (List<(int S, int E)> First, List<(int S, int E)> Second) BoundarySpans(string first, string second, RedactionOptions o)
    {
        var a = new List<(int S, int E)>();
        var b = new List<(int S, int E)>();
        int cut = first.Length + 1;
        foreach (var (s, e) in Redactor.TranscriptSpans(first + " " + second, o))
        {
            if (s < first.Length) a.Add((s, Math.Min(e, first.Length)));
            if (e > cut) b.Add((Math.Max(0, s - cut), e - cut));
        }
        return (a, b);
    }

    /// <summary>Maps a QPC time (100 ns) to wall clock using the current offset.</summary>
    public static DateTimeOffset QpcToWall(long qpc100ns)
    {
        long now = Capture.OutputCapture.Qpc100ns(System.Diagnostics.Stopwatch.GetTimestamp());
        return DateTimeOffset.UtcNow - TimeSpan.FromTicks(now - qpc100ns);
    }
}
