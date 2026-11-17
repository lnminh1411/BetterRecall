// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Diagnostics;
using BetterRecall.Agent.Audio;
using BetterRecall.Agent.Guardrails;
using BetterRecall.Agent.Native;
using BetterRecall.Agent.Replay;
using BetterRecall.Agent.Util;
using BetterRecall.Contracts;

namespace BetterRecall.Agent.Capture;

/// <summary>
/// Owns the per-output capture threads, the keyframe clock and event triggers, the replay engine
/// (one encoder for the composited canvas, or one per monitor, plus desktop audio) and replay saving.
/// </summary>
internal sealed class CaptureService : IAsyncDisposable
{
    private readonly Func<Settings> _settings;
    private readonly Func<bool> _keyframesPaused;
    private readonly Func<bool> _replayPaused;
    private readonly GuardrailService _guard;
    private readonly KeyframePipeline _pipeline;
    private readonly LoopbackHub _loopback;
    private readonly GpuDeviceCache _devices = new();
    private readonly ReplayEngine _engine;
    private readonly object _gate = new();
    private List<OutputCapture> _outputs = new();
    private int _layoutGen;   // bumped on every output rebuild, under _gate (A7)
    private AudioReplayEncoder? _audio;
    private Timer? _timer;
    private long _lastKeyframeTick;
    private long _lastSampleTick;
    private long _lastEventTrigger;
    private long _pendingTriggerAt;
    private uint _lastInputTick;
    private bool _inputBurst;
    private bool _lockMemory;

    private readonly Privacy.RedactionMasks? _redactions;

    public CaptureService(Func<Settings> settings, Func<bool> keyframesPaused, Func<bool> replayPaused,
        GuardrailService guard, KeyframePipeline pipeline, LoopbackHub loopback, bool lockMemory, Privacy.RedactionMasks? redactions = null)
    {
        _redactions = redactions;
        _settings = settings;
        _keyframesPaused = keyframesPaused;
        _replayPaused = replayPaused;
        _guard = guard;
        _pipeline = pipeline;
        _loopback = loopback;
        _lockMemory = lockMemory;
        _engine = new ReplayEngine(_devices, () => Outputs, Masks);
        if (pipeline is not null) pipeline.OcrDropped = RetryKeyframe;   // null in some tests
    }

    /// <summary>TH1: a grab of this monitor (DXGI index) was dropped because its OCR failed: grab it again at the
    /// next keyframe or sample tick, also when the screen stays still.</summary>
    private void RetryKeyframe(int monitor)
    {
        lock (_gate)
            foreach (var o in _outputs)
                if (o.Info.Index == monitor) o.RetryKeyframe();
    }

    /// <summary>Where the capture.monitors index list is pinned to stable monitor ids (null: not saved, tests).</summary>
    public string? MonitorSelectionFile { get; init; }

    public IReadOnlyList<OutputCapture> Outputs { get { lock (_gate) return _outputs.ToList(); } }
    public ReplayEngine Engine => _engine;
    public AudioReplayEncoder? Audio => _audio;
    internal GpuDeviceCache Devices => _devices;

    /// <summary>Monitor bounds for the guardrail scan, keyed by <see cref="GuardKey"/>.</summary>
    public IReadOnlyList<(int, RECT)> MonitorBounds()
    {
        lock (_gate)
            return _outputs.Select(o => (GuardKey(o), new RECT(o.Info.Left, o.Info.Top, o.Info.Left + o.Info.Width, o.Info.Top + o.Info.Height))).ToList();
    }

    /// <summary>Guard snapshot key of an output: the DXGI index plus the layout generation. A hot-plug renumbers DXGI
    /// indices, so a snapshot scanned before the rebuild must not apply its index 0 to the monitor that is index 0 now (A7);
    /// with the generation in the key it knows none of the new outputs and they are skipped until the next scan. Under _gate.</summary>
    private int GuardKey(OutputCapture o) => unchecked(_layoutGen * 64 + o.Info.Index);

    public void Start()
    {
        MediaFoundationRuntime.EnsureStarted();
        Rebuild();
        _engine.Start();
        _timer = new Timer(_ => Tick(), null, 250, 250);
    }

    /// <summary>Re-enumerates outputs (start, display change, capture.monitors change).</summary>
    public void Rebuild()
    {
        List<OutputCapture> old;
        lock (_gate) { old = _outputs; _outputs = new(); }
        foreach (var o in old) o.Dispose();
        var s = _settings();
        try
        {
            var all = GpuDevices.Outputs();
            // By stable monitor id, not by enumeration index: a hot-plug renumbers outputs (A12).
            var (outputs, pin) = MonitorSelection.Resolve(s.Capture.Monitors ?? [], all, MonitorSelection.StableId, MonitorSelection.Load(MonitorSelectionFile));
            if (pin is not null) MonitorSelection.Save(MonitorSelectionFile, pin);
            var list = outputs.Select(o => new OutputCapture(o, _devices, _pipeline.Post)).ToList();
            lock (_gate) { _outputs = list; _layoutGen++; }
            ApplySettings();
            foreach (var o in list) o.Start();
            _engine.RequestRebuild("outputs");
            Log.Info($"capturing {list.Count} output(s)");
        }
        catch (Exception ex) { Log.Error("capture rebuild", ex); }
        _guard.Refresh();
    }

    /// <summary>Replay masks: denied windows (guardrails), password fields (live, privacy.redactPasswords) and the
    /// redaction boxes of each monitor's newest keyframe.</summary>
    internal static readonly RECT MaskEverything = new(-(1 << 20), -(1 << 20), 1 << 20, 1 << 20);   // clipped to each output

    private RECT[] Masks()
    {
        var snap = _guard.Current();
        var s = _settings();
        var masks = MergeMasks(snap, s.Privacy.RedactPasswords, _redactions?.All() ?? [], _guard.UnscannedForeground(snap));
        // C2 (D2): text masks unchecked (OCR failing, or none since start): black the foreground window, or everything
        // when no foreground window is known. One volatile read per frame while OCR works.
        if (_pipeline is not { MasksUnchecked: true } || !TextMasksNeeded(s)) return masks;
        nint fg = WindowInspector.ForegroundRoot();
        var r = fg != 0 && WindowInspector.TryGetBounds(fg, out var b) ? b : MaskEverything;
        return [.. masks, r];
    }

    /// <summary>OCR and a text detector are on, so the replay relies on OCR text masks.</summary>
    private static bool TextMasksNeeded(Settings s) => s.Ocr.Enabled && Privacy.RedactionOptions.From(s.Privacy).AnyText;

    /// <summary>C2 (D2): the replay is recording while its text masks cannot be checked (status warning
    /// replay-text-unchecked); the foreground window is masked meanwhile.</summary>
    public bool ReplayTextUnchecked()
    {
        var s = _settings();
        return s.Replay.Enabled && !_engine.Paused && _pipeline is { MasksUnchecked: true } && TextMasksNeeded(s);
    }

    /// <param name="unscannedForeground">Bounds of a foreground window the snapshot has not scanned yet (A1): masked until
    /// a scan that started after the foreground change lands, as keyframes are refused meanwhile. Not the whole canvas,
    /// so an alt-tab does not black the replay.</param>
    internal static RECT[] MergeMasks(GuardSnapshot snap, bool passwords, RECT[] redactions, RECT? unscannedForeground = null)
    {
        // No fresh guard snapshot (guard thread stalled, or before its first scan): nothing says which windows are denied,
        // so black the whole canvas rather than record unmasked. Keyframes fail closed on the same snapshot.
        if (ReferenceEquals(snap, GuardSnapshot.Closed)) return [MaskEverything];
        var secrets = passwords ? snap.SecretRects : [];
        if (secrets.Length == 0 && redactions.Length == 0 && unscannedForeground is null) return snap.MaskRects;
        return unscannedForeground is { } fg ? [.. snap.MaskRects, .. secrets, .. redactions, fg] : [.. snap.MaskRects, .. secrets, .. redactions];
    }

    /// <summary>Pushes replay settings and pause state into the engine and the audio ring.</summary>
    public void ApplySettings()
    {
        var s = _settings();
        var r = s.Replay;
        int clampedSeconds = Math.Clamp(r.Seconds, 15, 600);
        _engine.Configure(new ReplayEngineConfig(
            r.Enabled,
            r.Layout,
            r.Fps,
            Math.Clamp(r.BitrateKbps, 500, 50000),
            clampedSeconds,
            r.Encoder,
            r.EncoderAdapter,
            r.Resolution,
            r.ScalePercent,
            _lockMemory));
        bool replayPaused = _replayPaused();
        _engine.Paused = replayPaused;
        SyncReplayAudio(s, replayPaused);
    }

    private readonly object _audioGate = new();
    // A15: a failed loopback start is retried with backoff (from the 250 ms tick), not on every pause change.
    private readonly RetryBackoff _audioRetry = new(2000, 120_000);

    private void SyncReplayAudio(Settings s, bool replayPaused)
    {
        var r = s.Replay;
        int clampedSeconds = Math.Clamp(r.Seconds, 15, 600);
        bool wantAudio = r.Enabled && r.Audio;
        lock (_audioGate)
        {
            if (wantAudio && (_audio is null || _audio.Seconds != clampedSeconds))
            {
                long now = Environment.TickCount64;
                if (_audio is not null || _audioRetry.Due(now))
                {
                    _audio?.Dispose();
                    _audio = null;
                    try { _audio = new AudioReplayEncoder(_loopback, clampedSeconds, _lockMemory, ReplayAudioMuted); _audioRetry.Succeeded(); }
                    catch (Exception ex) { Log.Warn($"replay audio unavailable (retry in {_audioRetry.Failed(now) / 1000} s)", ex); }
                }
            }
            else if (!wantAudio && _audio is not null)
            {
                _audio.Dispose();
                _audio = null;
            }
            if (!wantAudio) _audioRetry.Succeeded();
            if (_audio is not null) _audio.Paused = replayPaused;
        }
    }

    /// <summary>A-H2 (D1): replay audio is silence while a denied, private or protected window is in front, or while a
    /// foreground change is not scanned yet. Per audio packet; no Win32 call.</summary>
    private bool ReplayAudioMuted() => _guard.AudioNow().Gate != AudioGate.Allowed;

    public void SetLockMemory(bool on) => _lockMemory = on;

    /// <summary>Foreground change: capture once the new window has painted.</summary>
    public void OnForegroundChanged()
    {
        if (!_settings().Capture.EventTriggers) return;
        Interlocked.Exchange(ref _pendingTriggerAt, Environment.TickCount64 + 600);
    }

    private void Tick()
    {
        try
        {
            var s = _settings();
            long now = Environment.TickCount64;
            // Replay pause state follows lock/user pause/fullscreen without waiting for a settings change.
            bool replayPaused = _replayPaused();
            _engine.Paused = replayPaused;
            if (_audio is not null) _audio.Paused = replayPaused;
            else if (s.Replay.Enabled && s.Replay.Audio) SyncReplayAudio(s, replayPaused);   // retry a failed start (backoff inside)

            // Replay samples (replay.searchable) follow the replay buffer, but not through a governance pause: fullscreen,
            // battery and storage pause keyframes and inference while the replay may keep running, and a sample costs an
            // OCR pass and an encode (A9). A saved clip then starts from the last sample taken before the pause.
            bool keyframesPaused = _keyframesPaused();
            // While keyframes pause, sample grabs also keep the replay text masks fresh with replay.searchable off (C2).
            bool sampleDue = SampleDue(s.Replay, replayPaused, SamplesPaused?.Invoke() ?? false, _engine.Pipelines.Count, now - _lastSampleTick,
                maskRefresh: keyframesPaused && TextMasksNeeded(s));
            if (keyframesPaused)
            {
                if (sampleDue) { _lastSampleTick = now; RequestKeyframes(force: false, keyframe: false, sample: true); }
                return;
            }
            bool due = now - _lastKeyframeTick >= Math.Clamp(s.Capture.IntervalMs, 500, 60000);
            bool trigger = false;
            if (s.Capture.EventTriggers)
            {
                long pending = Interlocked.Read(ref _pendingTriggerAt);
                if (pending != 0 && now >= pending) { trigger = true; Interlocked.Exchange(ref _pendingTriggerAt, 0); }
                // Typing pause / scroll stop: input burst followed by 700 ms without input.
                uint idle = Win32.IdleMilliseconds();
                uint lastInput = unchecked(Win32.GetTickCount() - idle);
                if (lastInput != _lastInputTick) { _lastInputTick = lastInput; _inputBurst = true; }
                else if (_inputBurst && idle >= 700) { _inputBurst = false; trigger = true; }
                if (trigger && now - _lastEventTrigger < 1000) trigger = false;
            }
            if (!due && !trigger)
            {
                if (sampleDue) { _lastSampleTick = now; RequestKeyframes(force: false, keyframe: false, sample: true); }
                return;
            }
            if (due) _lastKeyframeTick = now;
            if (trigger) _lastEventTrigger = now;
            if (sampleDue) _lastSampleTick = now;
            RequestKeyframes(force: false, keyframe: true, sample: sampleDue, trigger: trigger);
        }
        catch (Exception ex) { Log.Warn("capture tick", ex); }
    }

    /// <summary>Applies the guardrails and asks each output for a keyframe.</summary>
    public void RequestKeyframes(bool force, bool keyframe = true, bool sample = false, bool trigger = false)
    {
        var s = _settings();
        // Only a snapshot scanned after the current foreground window came up: the tick fires every 250 ms, a scan can
        // take a second, and the user may have switched to a denied window in between (A4).
        var snap = _guard.CurrentForKeyframe();
        if (!snap.Decision.Allow) { SkippedByGuard++; LastGuardReason = snap.Decision.Reason; return; }
        LastGuardReason = GuardReason.None;
        var ts = DateTimeOffset.UtcNow;
        lock (_gate)
        {
            foreach (var o in _outputs)
            {
                // A monitor the snapshot did not scan (new after a rebuild) is unknown: skip it until the next scan (A12).
                if (!snap.PerMonitor.TryGetValue(GuardKey(o), out var mt) || mt.DeniedVisible) { SkippedByGuard++; continue; }
                var w = mt.Window ?? (snap.Foreground is { } fg && IsOnMonitor(fg, o.Info) ? fg : null);
                string exe = w?.ExePath ?? "";
                string app = exe.Length == 0 ? "" : Path.GetFileNameWithoutExtension(exe);
                string title = w?.Title ?? "";
                string site = w is not null && snap.Foreground is { } f && f.Hwnd == w.Hwnd ? snap.Site : "";
                var bounds = new RECT(o.Info.Left, o.Info.Top, o.Info.Left + o.Info.Width, o.Info.Top + o.Info.Height);
                o.PendingKeyframe = new KeyframeRequest(ts, app, exe, title, app + "\u0001" + title, force,
                    Math.Clamp(s.Capture.MaxWidth, 640, 3840),
                    // Pre-grab gate stays at 0.5 % of dirty area since the last grab; the real test
                    // (capture.dirtyMinPercent against the last stored keyframe) runs in the pipeline.
                    Math.Min(Math.Clamp(s.Capture.DirtyMinPercent, 0, 100), CapturePolicy.TriggerMinScore), site, keyframe, sample, trigger,
                    s.Privacy.RedactPasswords ? snap.SecretRects : null, bounds);
            }
        }
    }

    private static bool IsOnMonitor(WindowInfo w, OutputInfo o) =>
        WindowInspector.TryGetBounds(w.Hwnd, out var r) && r.Left < o.Left + o.Width && o.Left < r.Right && r.Top < o.Top + o.Height && o.Top < r.Bottom;

    /// <summary>True while samples must not run although the replay records. Samples (and their OCR) go on through the
    /// fullscreen, battery and storage pauses: they keep the replay text masks fresh (C2).</summary>
    public Func<bool>? SamplesPaused { get; init; }

    /// <summary>A replay sample grab is due. maskRefresh: due also with replay.searchable off (the grab only refreshes the
    /// replay text masks). Pure.</summary>
    internal static bool SampleDue(ReplaySettings r, bool replayPaused, bool samplesPaused, int pipelines, long sinceLastMs, bool maskRefresh = false) =>
        r.Enabled && (r.Searchable || maskRefresh) && !replayPaused && !samplesPaused && pipelines > 0
        && sinceLastMs >= Math.Clamp(r.SampleSeconds, 2, 60) * 1000L;

    public long SkippedByGuard;
    public GuardReason LastGuardReason;

    /// <summary>Writes the last replay.seconds of every pipeline plus desktop audio as fragmented MP4.</summary>
    public int SaveReplay(Stream output)
    {
        var s = _settings();
        return _engine.Save(output, _audio?.Source, Math.Clamp(s.Replay.Seconds, 15, 600));
    }

    /// <summary>A purge reached the replay window (A11): zero the video and audio rings and drop queued grabs, so a later
    /// replay save cannot write purged video. De-dup state and the replay redaction masks stay.</summary>
    public void ForgetRecent()
    {
        _engine.Panic();
        _audio?.Ring.Clear();
        _pipeline.Flush();
    }

    /// <summary>Panic: zero every ring and forget de-dup state. Replay redaction masks stay (A9).</summary>
    public void Panic()
    {
        _engine.Panic();
        _audio?.Ring.Clear();
        _pipeline.ResetDedup();
        _pipeline.Flush();
    }

    public long ReplayRingBytes()
    {
        return _engine.RingBytes() + (_audio?.Ring.CommittedBytes ?? 0);
    }

    public async ValueTask DisposeAsync()
    {
        if (_timer is not null) await _timer.DisposeAsync().ConfigureAwait(false);
        _engine.Dispose();
        List<OutputCapture> old;
        lock (_gate) { old = _outputs; _outputs = new(); }
        foreach (var o in old) o.Dispose();
        _audio?.Dispose();
        _devices.Dispose();
    }
}
