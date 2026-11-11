// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Diagnostics;
using System.Text.Json;
using BetterRecall.Agent.Audio;
using BetterRecall.Agent.Capture;
using BetterRecall.Agent.Guardrails;
using BetterRecall.Agent.Native;
using BetterRecall.Agent.Replay;
using BetterRecall.Agent.Scheduling;
using BetterRecall.Agent.Security;
using BetterRecall.Agent.Thumbs;
using BetterRecall.Agent.Util;
using BetterRecall.Agent.Worker;
using BetterRecall.Contracts;

namespace BetterRecall.Agent.Host;

internal sealed record AgentOptions(string InstallDir, string Home, bool Autostart);

/// <summary>Embeds search queries through the worker's text tower (kind "query").</summary>
internal sealed class QueryEmbedder(WorkerClient worker) : IQueryEmbedder
{
    public Task<float[]> EmbedQueryAsync(string text, CancellationToken ct) => worker.EmbedTextAsync(text, "query", ct);
}

/// <summary>
/// Composition root and IAgentControl: wires Core, capture, replay, audio, worker, scheduler, guardrails,
/// tray, hotkeys and the read gate. Runs the message loop on the calling (STA) thread.
/// </summary>
internal sealed class AgentHost : IAgentControl, IAsyncDisposable
{
    private const int HkSearch = 1, HkPause = 2, HkReplay = 3, HkPanic = 4, HkTimeline = 5, HkHome = 6, HkLock = 7;
    private volatile string[]? _hotkeysFailed;

    private readonly ICoreHost _core;
    private readonly AgentOptions _opt;
    private readonly ReadGate _gate = new();
    private readonly PauseController _pause = new();
    private readonly WorkerSupervisor _supervisor;
    private readonly WorkerClient _worker;
    private readonly Thumbnailer _thumbs;
    private readonly LoopbackHub _loopback = new();
    private readonly ReplaySampler _sampler = new();
    private readonly SemaphoreSlim _benchLock = new(1, 1);
    private readonly CancellationTokenSource _cts = new();
    private MessageWindow? _window;
    private EventWaitHandle? _quitEvent;
    private RegisteredWaitHandle? _quitWait;
    private GuardrailService? _guard;
    private KeyframePipeline? _pipeline;
    private CaptureService? _capture;
    private EmbeddingScheduler? _scheduler;
    private SystemProbe? _probe;
    private ReplayLibrary? _replays;
    private TranscriptSource? _systemAudio, _micAudio;
    private Transcriber? _transcriber;
    private Timer? _statusTimer, _maintenanceTimer, _storageTimer;
    private StorageGuard? _storage;
    private bool _sessionLocked;
    private bool _captureStarted;
    private bool _lateIsolationDone;
    private string _lastStatusKey = "";
    private (StoreStats Stats, long At) _statsCache;
    private Settings _settingsSnapshot;
    private Task? _openStore;

    public event Action<AgentStatus>? StatusChanged;
    public event Action<CaptureMeta>? CaptureStored;

    public AgentHost(ICoreHost core, AgentOptions opt)
    {
        _core = core;
        _opt = opt;
        _settingsSnapshot = core.Settings.Current;
        var workerExe = InstallLayout.Full(opt.InstallDir, InstallLayout.WorkerExe);
        _supervisor = new WorkerSupervisor(new ProcessWorkerLauncher(workerExe, S.Governance.EcoQos), opt.Home, () => S.Inference.IdleExitS);
        _worker = new WorkerClient(_supervisor);
        _thumbs = new Thumbnailer(() => _core.Store, () => S);
    }

    private Settings S => _settingsSnapshot;
    private IStore? Store => _core.Store;

    // ---------------- startup ----------------

    public void Run()
    {
        var s = S;
        Log.SetLevel(s.Log.Level);
        if (s.Security.MemoryIsolation) MemoryIsolation.ApplyEarly(_opt.InstallDir);
        if (s.Governance.EcoQos) Governance.ApplyEcoQos(Win32.GetCurrentProcess(), idlePriority: false);

        _window = new MessageWindow();
        // Another version's launcher asks us to step aside (see PipeNames.AgentQuitEvent).
        _quitEvent = new EventWaitHandle(false, EventResetMode.AutoReset, PipeNames.AgentQuitEvent);
        _quitWait = ThreadPool.RegisterWaitForSingleObject(_quitEvent, (_, _) => { Log.Info("quit requested by launcher"); RequestQuit(); }, null, Timeout.Infinite, executeOnlyOnce: true);
        _window.Hotkey += OnHotkey;
        _window.Command += OnTrayCommand;
        _window.BalloonClicked += () => UiLauncher.Start(_opt.InstallDir, "--replays");
        _window.SessionChanged += OnSessionChanged;
        _window.DisplayChanged += () => _capture?.Rebuild();
        _window.MenuState = () => (StatusLine(), _pause.Current().Reason is "user" or "ipc", _capture?.Engine.Pipelines.Count > 0);
        _window.ShowTray("BetterRecall");
        RegisterHotkeys();
        _core.Settings.Changed += OnSettingsChanged;

        _replays = new ReplayLibrary(_opt.Home, () => S, _core.Blobs);
        var encLuid = GpuDevices.Adapters().FirstOrDefault(a => a.VendorId == GpuDevices.VendorIntel)?.Luid;
        _probe = new SystemProbe(encLuid is null ? null : GpuDevices.PdhLuidToken(encLuid.Value));
        _scheduler = new EmbeddingScheduler(() => Store, () => S, _worker, ProbeLoad, () => _pause.Has("consent") || _pause.Has("keylocked"));
        _pipeline = new KeyframePipeline(() => Store, () => S, OnCaptureStored, () => _capture?.Devices) { Sampler = _sampler };
        _guard = new GuardrailService(() => S, () => _sessionLocked, () => _capture?.MonitorBounds() ?? [], _opt.InstallDir);
        _guard.ForegroundChanged += _ => _capture?.OnForegroundChanged();
        _window.ForegroundChanged += () => _guard?.OnForegroundEvent();
        _transcriber = new Transcriber(_worker, () => Store, () => S, () => _guard?.Current().Foreground?.ProcessName.Replace(".exe", "", StringComparison.OrdinalIgnoreCase) ?? "", () => _scheduler?.Signal());

        _core.StartPipes(this, new QueryEmbedder(_worker), _thumbs);
        if (!s.Capture.Consented) _pause.Set("consent");
        _pause.Set("keylocked");
        _openStore = Task.Run(OpenStoreAsync);
        _pause.Changed += () => _capture?.ApplySettings();

        _guard.Start();
        _scheduler.Start();
        _statusTimer = new Timer(_ => StatusTick(), null, 1000, 1000);
        _maintenanceTimer = new Timer(_ => Maintenance(), null, TimeSpan.FromHours(1), TimeSpan.FromHours(1));
        _storage = new StorageGuard(() => Store, () => S, () => StorageGuard.FreeBytesOf(_opt.Home));
        _storageTimer = new Timer(_ => StorageCheck(), null, TimeSpan.FromSeconds(5), TimeSpan.FromSeconds(30));
        if (_opt.Autostart)
        {
            try { StartupRegistration.Register(_opt.InstallDir); } catch (Exception ex) { Log.Warn("startup registration", ex); }
        }
        if (s.Capture.Consented) StartCapture();
        if (!s.Ui.StartMinimized) UiLauncher.Start(_opt.InstallDir, "--page", "home");   // ui.startMinimized off: open Home with the tray icon
        _ = Task.Delay(TimeSpan.FromSeconds(15)).ContinueWith(_ => Diagnostics.Mark(), TaskScheduler.Default);
        Log.Info("agent running");
        _window.Run();
        Log.Info("message loop ended");
    }

    private async Task OpenStoreAsync()
    {
        try
        {
            var sw = Stopwatch.StartNew();
            await _core.OpenStoreAsync(_cts.Token).ConfigureAwait(false);
            Log.Info($"store open in {sw.ElapsedMilliseconds} ms ({_core.Keys.Protection})");
            _pause.Clear("keylocked");
            Maintenance();
            _scheduler?.Signal();
            RaiseStatus(force: true);
        }
        catch (OperationCanceledException) { }
        catch (Exception ex) { Log.Error("store open failed", ex); }
    }

    private void StartCapture()
    {
        if (_captureStarted) return;
        _captureStarted = true;
        var s = S;
        bool lockMem = s.Security.MemoryIsolation;
        if (lockMem)
        {
            // Pin the replay rings: per monitor bitrate * window, three monitors at most, plus audio.
            long ring = (long)s.Replay.BitrateKbps * 1000 / 8 * (s.Replay.Seconds + 2) * 3 + 4L * 1024 * 1024;
            MemoryIsolation.ReserveWorkingSet(ring);
        }
        _capture = new CaptureService(() => S, KeyframesPaused, ReplayPaused, _guard!, _pipeline!, _loopback, lockMem);
        _capture.Start();
        ImageCodec.Preload();
        SyncAudioSources();
        // ACG goes last: give the capture threads time to load drivers, encoders and the audio stack.
        _ = Task.Run(async () =>
        {
            await Task.Delay(TimeSpan.FromSeconds(8)).ConfigureAwait(false);
            if (S.Security.MemoryIsolation && !_lateIsolationDone)
            {
                _lateIsolationDone = true;
                MemoryIsolation.ApplyLate(dynamicCode: Environment.GetEnvironmentVariable("BETTERRECALL_NO_ACG") != "1");
            }
        });
    }

    private bool KeyframesPaused() =>
        _pause.KeyframesPaused || Store is null || !S.Capture.Enabled || !S.Capture.Consented;

    private bool ReplayPaused() =>
        _pause.ContinuousPaused(S.Replay.DuringFullscreen) || !S.Capture.Consented;

    private void SyncAudioSources()
    {
        if (!_captureStarted) return;
        var s = S;
        bool wantSystem = s.Audio.System && s.Capture.Consented;
        bool wantMic = s.Audio.Mic && s.Capture.Consented;
        if (wantSystem && _systemAudio is null)
        {
            try
            {
                _systemAudio = new TranscriptSource("system", _loopback, () => S, () => _pause.ContinuousPaused(true));
                _ = _transcriber!.RunAsync(_systemAudio, _cts.Token);
            }
            catch (Exception ex) { Log.Warn("system audio", ex); }
        }
        else if (!wantSystem && _systemAudio is not null) { _systemAudio.Dispose(); _systemAudio = null; }
        if (wantMic && _micAudio is null)
        {
            try
            {
                _micAudio = new TranscriptSource("mic", null, () => S, () => _pause.ContinuousPaused(true));
                _ = _transcriber!.RunAsync(_micAudio, _cts.Token);
            }
            catch (Exception ex) { Log.Warn("microphone", ex); }
        }
        else if (!wantMic && _micAudio is not null) { _micAudio.Dispose(); _micAudio = null; }
    }

    private void RegisterHotkeys()
    {
        var h = S.Hotkeys;
        var failed = new List<string>();
        var all = new[]
        {
            (HkSearch, "search", h.Search), (HkPause, "pause", h.Pause), (HkReplay, "replay", h.Replay), (HkPanic, "panic", h.Panic),
            (HkTimeline, "timeline", h.Timeline), (HkHome, "home", h.Home), (HkLock, "lock", h.Lock),
        };
        // Unregister first, so a combination moved from one action to another is free when the second registers.
        foreach (var (id, _, _) in all) _window!.UnregisterHotkey(id);
        foreach (var (id, name, text) in all)
        {
            if (string.IsNullOrWhiteSpace(text)) continue;   // not assigned
            var hk = HotkeyParser.Parse(text);
            if (hk is null) { Log.Warn($"hotkey {name} invalid"); failed.Add(name); continue; }
            if (!_window!.RegisterHotkey(id, name, hk.Value)) failed.Add(name);
        }
        _hotkeysFailed = failed.Count > 0 ? [.. failed] : null;
    }

    // ---------------- events ----------------

    private void OnSettingsChanged(Settings s)
    {
        var old = _settingsSnapshot;
        _settingsSnapshot = s;
        Log.SetLevel(s.Log.Level);
        _window?.Invoke(() =>
        {
            if (!HotkeysEqual(old.Hotkeys, s.Hotkeys)) RegisterHotkeys();
            if (s.Capture.Consented && !old.Capture.Consented) { _pause.Clear("consent"); StartCapture(); }
            if (!old.Capture.Monitors.SequenceEqual(s.Capture.Monitors)) _capture?.Rebuild();
            else _capture?.ApplySettings();
            SyncAudioSources();
            RaiseStatus(force: true);
        });
        if (!StorageEqual(old.Storage, s.Storage)) _ = Task.Run(StorageCheck);
    }

    private static bool StorageEqual(StorageSettings a, StorageSettings b) =>
        a.MinFreeGb == b.MinFreeGb && a.MaxGb == b.MaxGb && a.Overwrite == b.Overwrite;

    private readonly object _storageGate = new();

    /// <summary>storage.minFreeGb / maxGb / overwrite: pause with reason "storage" (tray balloon once) or make room.</summary>
    private void StorageCheck()
    {
        if (!Monitor.TryEnter(_storageGate)) return;
        try
        {
            if (_storage is null || Store is null) return;
            var d = _storage.Check();
            if (d.Deleted > 0) { _statsCache = default; Log.Info($"storage: {d.Deleted} captures deleted to make room"); }
            if (d.Pause && !_pause.Has("storage"))
            {
                _pause.Set("storage");
                Log.Warn($"capture paused: storage ({d.Why})");
            }
            else if (!d.Pause && _pause.Has("storage"))
            {
                _pause.Clear("storage");
                Log.Info("capture resumed: storage back within limits");
            }
            if (d.Notify)
            {
                var st = S.Storage;
                _window?.Notify("BetterRecall paused", d.Why == "disk"
                    ? $"Less than {st.MinFreeGb} GB free on the data drive. Capture is paused until there is room."
                    : $"Stored captures reached {st.MaxGb} GB. Capture is paused; raise the limit or turn on overwrite.");
            }
            RaiseStatus(force: false);
        }
        catch (Exception ex) { Log.Warn("storage check", ex); }
        finally { Monitor.Exit(_storageGate); }
    }

    private static bool HotkeysEqual(HotkeySettings a, HotkeySettings b) =>
        a.Search == b.Search && a.Pause == b.Pause && a.Replay == b.Replay && a.Panic == b.Panic
        && a.Timeline == b.Timeline && a.Home == b.Home && a.Lock == b.Lock;

    private void OnHotkey(string name)
    {
        Log.Info($"hotkey {name}");
        switch (name)
        {
            case "search":
                _ = _worker.PrewarmAsync(CancellationToken.None).ContinueWith(t => { _ = t.Exception; }, TaskScheduler.Default);
                UiLauncher.ShowLauncher(_opt.InstallDir);
                break;
            case "pause":
                if (_pause.Has("user") || _pause.Has("ipc")) Resume("user");
                else Pause(null, "user");
                break;
            case "replay":
                _ = SaveReplayFromHotkeyAsync();
                break;
            case "panic":
                Panic();
                break;
            case "timeline":
                UiLauncher.Start(_opt.InstallDir, "--page", "timeline");
                break;
            case "home":
                UiLauncher.Start(_opt.InstallDir, "--page", "home");
                break;
            case "lock":
                Lock();
                break;
        }
    }

    private async Task SaveReplayFromHotkeyAsync()
    {
        try { await SaveReplayAsync(CancellationToken.None).ConfigureAwait(false); }
        catch (Exception ex)
        {
            Log.Warn("replay save", ex);
            _window?.Notify("BetterRecall", "Replay could not be saved.");
        }
    }

    private void OnTrayCommand(TrayCommand cmd)
    {
        switch (cmd)
        {
            case TrayCommand.Search: OnHotkey("search"); break;
            case TrayCommand.Pause15: Pause(TimeSpan.FromMinutes(15), "user"); break;
            case TrayCommand.Pause60: Pause(TimeSpan.FromHours(1), "user"); break;
            case TrayCommand.PauseUntilResume: Pause(null, "user"); break;
            case TrayCommand.Resume: Resume("user"); break;
            case TrayCommand.SaveReplay: _ = SaveReplayFromHotkeyAsync(); break;
            case TrayCommand.Panic: Panic(); break;
            case TrayCommand.Settings: UiLauncher.Start(_opt.InstallDir, "--settings"); break;
            case TrayCommand.Quit: _window?.Quit(); break;
        }
    }

    private void OnSessionChanged(int code)
    {
        switch (code)
        {
            case Win32.WTS_SESSION_LOCK:
            case Win32.WTS_CONSOLE_DISCONNECT:
            case Win32.WTS_REMOTE_DISCONNECT:
                _sessionLocked = true;
                if (S.Privacy.PauseOnLock) _pause.Set("lock");
                if (S.Security.LockOnSessionLock) _gate.Close();
                break;
            case Win32.WTS_SESSION_UNLOCK:
            case Win32.WTS_CONSOLE_CONNECT:
                _sessionLocked = false;
                _pause.Clear("lock");
                break;
        }
        _guard?.Refresh();
        RaiseStatus(force: true);
    }

    private void OnCaptureStored(CaptureMeta meta)
    {
        _scheduler?.Signal();
        try { CaptureStored?.Invoke(meta); } catch (Exception ex) { Log.Warn("CaptureStored handler", ex); }
    }

    private GovernanceInputs ProbeLoad()
    {
        var p = _probe!;
        return new GovernanceInputs(p.CpuPercent(), p.IgpuPercent(), SystemProbe.OnBattery(), SystemProbe.Notification(), Win32.IdleMilliseconds());
    }

    private void StatusTick()
    {
        try
        {
            // Fullscreen D3D or presentation mode pauses keyframes and inference.
            var n = SystemProbe.Notification();
            if (Governance.PauseKeyframes(n, S.Capture)) { if (!_pause.Has("fullscreen")) _pause.Set("fullscreen"); }
            else _pause.Clear("fullscreen");
            RaiseStatus(force: false);
            _window?.Invoke(() => _window.UpdateTip("BetterRecall: " + StatusLine()));
        }
        catch (Exception ex) { Log.Debug($"status tick {ex.Message}"); }
    }

    private string StatusLine()
    {
        var (reason, until) = _pause.Current();
        if (reason is null) return _captureStarted ? "Capturing" : "Starting";
        return reason switch
        {
            "consent" => "Waiting for consent",
            "keylocked" => "Locked: enter passphrase",
            "lock" => "Paused: session locked",
            "fullscreen" => "Paused: fullscreen",
            "storage" => "Paused: storage limit",
            _ => until is { } u ? $"Paused until {u.ToLocalTime():HH:mm}" : "Paused",
        };
    }

    private void RaiseStatus(bool force)
    {
        var st = GetStatus();
        var key = $"{st.Capturing}|{st.Paused}|{st.PauseReason}|{st.PausedUntil}|{st.Locked}|{st.Device}|{st.Queue.Pending}|{st.Audio}";
        if (!force && key == _lastStatusKey) return;
        _lastStatusKey = key;
        try { StatusChanged?.Invoke(st); } catch (Exception ex) { Log.Warn("StatusChanged handler", ex); }
    }

    private void Maintenance()
    {
        try
        {
            var store = Store;
            if (store is null) return;
            var sw = Stopwatch.StartNew();
            store.RunMaintenance(S);
            Log.Info($"maintenance done in {sw.ElapsedMilliseconds} ms");
            StorageCheck();
        }
        catch (Exception ex) { Log.Warn("maintenance", ex); }
    }

    // ---------------- IAgentControl ----------------

    public AgentStatus GetStatus()
    {
        var (reason, until) = _pause.Current();
        bool paused = reason is not null;
        var s = S;
        return new AgentStatus(
            Capturing: _captureStarted && !paused && s.Capture.Enabled,
            Paused: paused,
            PausedUntil: until,
            PauseReason: reason,
            Locked: !IsUnlocked,
            UnlockedUntil: _gate.Until,
            Audio: new AudioState(_systemAudio is not null, _micAudio is not null),
            Queue: new QueueState((_scheduler?.Queued ?? 0) + StoreStats().PendingEmbeddings),
            Device: _supervisor.IsRunning ? _supervisor.Hello?.Device ?? "none" : "none",
            Store: StoreStats(),
            RetentionDays: s.Retention.Days,
            HotkeysFailed: _hotkeysFailed);
    }

    private StoreStats StoreStats()
    {
        long now = Environment.TickCount64;
        if (_statsCache.Stats is not null && now - _statsCache.At < 5000) return _statsCache.Stats;
        StoreStats st;
        try { st = Store?.Stats() ?? new StoreStats(0, 0, 0, null, null, 0, 0); }
        catch { st = new StoreStats(0, 0, 0, null, null, 0, 0); }
        _statsCache = (st, now);
        return st;
    }

    public void Pause(TimeSpan? duration, string reason)
    {
        var r = reason == "ipc" ? "ipc" : "user";
        _pause.Set(r, duration is { } d ? DateTimeOffset.UtcNow + d : null);
        Log.Info($"paused ({r}) {(duration is null ? "until resume" : $"for {duration.Value.TotalMinutes:F0} min")}");
        RaiseStatus(force: true);
    }

    public void Resume(string reason)
    {
        _pause.Clear("user");
        _pause.Clear("ipc");
        Log.Info("resumed");
        RaiseStatus(force: true);
    }

    public bool IsUnlocked => _gate.IsOpen(S.Security.RequireUnlock);

    public async Task<bool> UnlockAsync(CancellationToken ct)
    {
        if (!S.Security.RequireUnlock) return true;
        bool ok;
        try { ok = await _core.Keys.VerifyUserAsync(_window?.Handle ?? 0, "Unlock BetterRecall to view your history", ct).ConfigureAwait(false); }
        catch (Exception ex) { Log.Warn("Windows Hello", ex); ok = false; }
        if (ok) _gate.Open(S.Security.UnlockTimeoutMin);
        RaiseStatus(force: true);
        return ok;
    }

    /// <summary>Core verified the unlock password (or the data-key passphrase): open the gate without Hello.</summary>
    public void OpenReadGate()
    {
        _gate.Open(S.Security.UnlockTimeoutMin);
        RaiseStatus(force: true);
    }

    public void Lock()
    {
        _gate.Close();
        _thumbs.ClearCache();
        RaiseStatus(force: true);
    }

    public void Consent()
    {
        using var doc = JsonDocument.Parse("{\"capture\":{\"consented\":true}}");
        var err = _core.Settings.TryUpdate(doc.RootElement);
        if (err is not null) throw new InvalidOperationException(err);
        // Settings.Changed starts capture; do it here as well in case Core does not raise the event synchronously.
        _settingsSnapshot = _core.Settings.Current;
        _window?.Invoke(() => { _pause.Clear("consent"); StartCapture(); RaiseStatus(force: true); });
    }

    public async Task<ReplayClip> SaveReplayAsync(CancellationToken ct)
    {
        var capture = _capture ?? throw new InvalidOperationException("Replay is not running.");
        var lib = _replays!;
        var ts = DateTimeOffset.UtcNow;
        var id = ReplayLibrary.NewId(ts);
        var (path, encrypted) = lib.TargetFor(id);
        Directory.CreateDirectory(Path.GetDirectoryName(path)!);
        var partial = path + ".partial";
        var sw = Stopwatch.StartNew();
        int durationMs = await Task.Run(() =>
        {
            using var stream = encrypted ? _core.Blobs.OpenWrite(partial) : new FileStream(partial, FileMode.Create, FileAccess.Write, FileShare.None);
            return capture.SaveReplay(stream);
        }, ct).ConfigureAwait(false);
        File.Move(partial, path, overwrite: true);
        long bytes = new FileInfo(path).Length;
        Log.Info($"replay saved: {durationMs} ms, {bytes / 1024} KiB, encrypted={encrypted}, {sw.ElapsedMilliseconds} ms");
        if (S.Replay.Notify) _window?.Notify("Replay saved", $"Last {durationMs / 1000} s saved. Click to open the replay list.");
        IndexReplayFrames(id, ts, durationMs);
        return new ReplayClip(id, ts, durationMs, bytes, encrypted, path);
    }

    /// <summary>replay.searchable: the sampled frames of the saved clip go into the store as kind replay (id + offset).</summary>
    private void IndexReplayFrames(string id, DateTimeOffset end, int durationMs)
    {
        var s = S;
        if (!s.Replay.Searchable || Store is not { } store) return;
        var start = end - TimeSpan.FromMilliseconds(durationMs);
        var samples = _sampler.Take(start, end, Math.Clamp(s.Replay.SampleSeconds, 2, 60));
        int n = 0;
        try
        {
            foreach (var f in ReplaySampler.ToFrames(id, start, durationMs, samples))
            {
                try { store.AddReplayFrame(f); n++; }
                finally { Array.Clear(f.Image); }
            }
        }
        catch (Exception ex) { Log.Warn("replay frames", ex); }
        if (n > 0) _scheduler?.Signal();
        Log.Info($"replay {id}: {n} frames queued for search");
    }

    public IReadOnlyList<ReplayClip> ListReplays() => _replays?.List() ?? [];

    public Task ExportReplayAsync(string id, string path, CancellationToken ct) => _replays!.ExportAsync(id, path, ct);

    public void DeleteReplay(string id)
    {
        _replays!.Delete(id);
        try { Store?.DeleteReplayFrames(id); } catch (Exception ex) { Log.Warn("replay frames delete", ex); }
    }

    public void Panic()
    {
        var sw = Stopwatch.StartNew();
        _capture?.Panic();
        _sampler.Clear();
        _scheduler?.Clear();
        _thumbs.ClearCache();
        _systemAudio?.Clear();
        _micAudio?.Clear();
        _gate.Close();
        int purged = 0;
        int minutes = Math.Max(0, S.Privacy.PanicPurgeMinutes);
        if (Store is { } store)
        {
            // With 0 minutes Core deletes nothing and still runs wal_checkpoint(TRUNCATE).
            try { purged = store.Purge(PurgeScope.Last(TimeSpan.FromMinutes(minutes))); }
            catch (Exception ex) { Log.Warn("panic purge", ex); }
        }
        Log.Info($"panic flush in {sw.ElapsedMilliseconds} ms, purged {purged} rows");
        _window?.Notify("BetterRecall", minutes > 0 ? $"Memory flushed, last {minutes} min deleted." : "Memory flushed.");
        RaiseStatus(force: true);
    }

    public Task<BenchResult> BenchAsync(CancellationToken ct) => _worker.BenchAsync(ct);

    public async Task<BenchReport> BenchFullAsync(CancellationToken ct)
    {
        var capture = _capture ?? throw new InvalidOperationException("Capture is not running.");
        if (!await _benchLock.WaitAsync(0).ConfigureAwait(false))
            throw new InvalidOperationException("A benchmark is already running.");
        try
        {
            var target = new AgentBenchTarget(capture, async ct2 => (BenchResult?)await _worker.BenchAsync(ct2).ConfigureAwait(false), () => S, capture.Devices);
            var report = await Task.Run(() => BenchRunner.RunAsync(target, TimeSpan.FromSeconds(2.5), () => DateTimeOffset.UtcNow, ct), ct).ConfigureAwait(false);
            Log.Info($"full benchmark completed in {report.TookMs} ms with {report.Encoders.Length} encoder results");
            return report;
        }
        finally
        {
            _benchLock.Release();
        }
    }

    // ---------------- diagnostics ----------------

    internal void RequestQuit() => _window?.Quit();

    internal CaptureService? Capture => _capture;
    internal KeyframePipeline? Pipeline => _pipeline;
    internal EmbeddingScheduler? Scheduler => _scheduler;
    internal GuardrailService? Guard => _guard;
    internal WorkerSupervisor Supervisor => _supervisor;

    public async ValueTask DisposeAsync()
    {
        _quitWait?.Unregister(null);
        _quitEvent?.Dispose();
        _cts.Cancel();
        if (_statusTimer is not null) await _statusTimer.DisposeAsync().ConfigureAwait(false);
        if (_maintenanceTimer is not null) await _maintenanceTimer.DisposeAsync().ConfigureAwait(false);
        if (_storageTimer is not null) await _storageTimer.DisposeAsync().ConfigureAwait(false);
        try { await _core.StopPipesAsync().ConfigureAwait(false); } catch (Exception ex) { Log.Warn("stop pipes", ex); }
        if (_capture is not null) await _capture.DisposeAsync().ConfigureAwait(false);
        _systemAudio?.Dispose();
        _micAudio?.Dispose();
        _loopback.Dispose();
        if (_pipeline is not null) await _pipeline.DisposeAsync().ConfigureAwait(false);
        if (_scheduler is not null) await _scheduler.DisposeAsync().ConfigureAwait(false);
        await _supervisor.DisposeAsync().ConfigureAwait(false);
        _guard?.Dispose();
        _probe?.Dispose();
        _thumbs.ClearCache();
        _window?.Dispose();
        _benchLock.Dispose();
    }
}
