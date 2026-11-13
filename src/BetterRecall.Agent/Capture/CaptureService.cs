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
    }

    public IReadOnlyList<OutputCapture> Outputs { get { lock (_gate) return _outputs.ToList(); } }
    public ReplayEngine Engine => _engine;
    public AudioReplayEncoder? Audio => _audio;
    internal GpuDeviceCache Devices => _devices;

    /// <summary>Monitor bounds for the guardrail scan.</summary>
    public IReadOnlyList<(int, RECT)> MonitorBounds()
    {
        lock (_gate)
            return _outputs.Select(o => (o.Info.Index, new RECT(o.Info.Left, o.Info.Top, o.Info.Left + o.Info.Width, o.Info.Top + o.Info.Height))).ToList();
    }

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
            var outputs = GpuDevices.Outputs();
            if (s.Capture.Monitors.Length > 0) outputs = outputs.Where(o => s.Capture.Monitors.Contains(o.Index)).ToList();
            var list = outputs.Select(o => new OutputCapture(o, _devices, _pipeline.Post)).ToList();
            lock (_gate) _outputs = list;
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

    private RECT[] Masks() => MergeMasks(_guard.Current(), _settings().Privacy.RedactPasswords, _redactions?.All() ?? []);

    internal static RECT[] MergeMasks(GuardSnapshot snap, bool passwords, RECT[] redactions)
    {
        // No fresh guard snapshot (guard thread stalled, or before its first scan): nothing says which windows are denied,
        // so black the whole canvas rather than record unmasked. Keyframes fail closed on the same snapshot.
        if (ReferenceEquals(snap, GuardSnapshot.Closed)) return [MaskEverything];
        var secrets = passwords ? snap.SecretRects : [];
        if (secrets.Length == 0 && redactions.Length == 0) return snap.MaskRects;
        return [.. snap.MaskRects, .. secrets, .. redactions];
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

        bool wantAudio = r.Enabled && r.Audio;
        if (wantAudio && (_audio is null || _audio.Seconds != clampedSeconds))
        {
            _audio?.Dispose();
            _audio = null;
            try { _audio = new AudioReplayEncoder(_loopback, clampedSeconds, _lockMemory); }
            catch (Exception ex) { Log.Warn("replay audio unavailable", ex); }
        }
        else if (!wantAudio && _audio is not null)
        {
            _audio.Dispose();
            _audio = null;
        }
        if (_audio is not null) _audio.Paused = replayPaused;
    }

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

            // Replay samples (replay.searchable) follow the replay buffer, not the keyframe pause: fullscreen pauses
            // keyframes but the replay may keep running (replay.duringFullscreen).
            bool sampleDue = s.Replay.Enabled && s.Replay.Searchable && !replayPaused && _engine.Pipelines.Count > 0
                && now - _lastSampleTick >= Math.Clamp(s.Replay.SampleSeconds, 2, 60) * 1000L;
            if (_keyframesPaused())
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
        var snap = _guard.Current();
        if (!snap.Decision.Allow) { SkippedByGuard++; LastGuardReason = snap.Decision.Reason; return; }
        LastGuardReason = GuardReason.None;
        var ts = DateTimeOffset.UtcNow;
        lock (_gate)
        {
            foreach (var o in _outputs)
            {
                if (!snap.PerMonitor.TryGetValue(o.Info.Index, out var mt)) mt = new MonitorTop(o.Info.Index, null, false);
                if (mt.DeniedVisible) { SkippedByGuard++; continue; }
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

    public long SkippedByGuard;
    public GuardReason LastGuardReason;

    /// <summary>Writes the last replay.seconds of every pipeline plus desktop audio as fragmented MP4.</summary>
    public int SaveReplay(Stream output)
    {
        var s = _settings();
        return _engine.Save(output, _audio?.Source, Math.Clamp(s.Replay.Seconds, 15, 600));
    }

    /// <summary>Panic: zero every ring and forget de-dup state.</summary>
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
