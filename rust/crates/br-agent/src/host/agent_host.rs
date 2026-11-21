// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Composition root and `br_core::AgentControl` implementation (`AgentHost.cs`).

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use br_contracts::{
    AgentStatus, BenchReport, BenchResult, CaptureMeta, HotkeySettings, InstallLayout, PipeNames,
    PrivacySettings, PurgeNotice, PurgeScope, QueueState, ReplayClip, Settings, StorageSettings,
    StoreStats,
};
use br_core::{
    AgentControl, AgentError, CancelSource, CancelToken, CoreHost, EncryptedStore, KeyVault,
    ListenerGuard, SettingsStore, StoreError, Thumbnailer,
};
use chrono::{DateTime, Duration as ChronoDuration, Local, Utc};
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::System::Threading::{
    CreateEventW, GetCurrentProcess, INFINITE, SetEvent, WaitForMultipleObjects,
};
use windows::core::PCWSTR;

use crate::guardrails::GuardrailService;
use crate::host::agent_state::{PauseController, ReadGate};
use crate::host::diagnostics::Diagnostics;
use crate::host::hardware_probe::HardwareProbe;
use crate::host::hotkey_parser::HotkeyParser;
use crate::host::message_window::{MenuStateSnapshot, MessageWindow, TrayCommand};
use crate::host::shell::{StartupRegistration, UiLauncher};
use crate::host::storage_guard::{StorageGuard, StorageStore};
use crate::native::{
    WTS_CONSOLE_CONNECT, WTS_CONSOLE_DISCONNECT, WTS_REMOTE_CONNECT, WTS_REMOTE_DISCONNECT,
    WTS_SESSION_LOCK, WTS_SESSION_UNLOCK, session_locked_now, tick_count64, to_wide_z,
};
use crate::scheduling::{
    EmbeddingScheduler, EmbeddingStore, Governance, GovernanceInputs, SystemProbe,
};
use crate::seams::{
    AudioSubsystem, CaptureSubsystem, KeyframeSubsystem, ReplaySubsystem, UnavailableAudio,
    UnavailableCapture, UnavailableKeyframes, UnavailableReplay, UnavailableThumbnailer,
};
use crate::security::MemoryIsolation;
use crate::util::AgentLog;
use crate::worker::{
    ProcessWorkerLauncher, QueryEmbedderImpl, VcRuntime, WorkerClient, WorkerError,
    WorkerSettingsDiff, WorkerSupervisor,
};

const HK_SEARCH: i32 = 1;
const HK_PAUSE: i32 = 2;
const HK_REPLAY: i32 = 3;
const HK_PANIC: i32 = 4;
const HK_TIMELINE: i32 = 5;
const HK_HOME: i32 = 6;
const HK_LOCK: i32 = 7;

#[derive(Debug, Clone)]
pub struct AgentOptions {
    pub install_dir: PathBuf,
    pub home: PathBuf,
    pub autostart: bool,
}

type StatusListener = Arc<dyn Fn(&AgentStatus) + Send + Sync>;
type CaptureListener = Arc<dyn Fn(&CaptureMeta) + Send + Sync>;
type PanicListener = Arc<dyn Fn() + Send + Sync>;

struct QuitEventWatcher {
    stop_event: HANDLE,
    thread: Option<JoinHandle<()>>,
}

unsafe impl Send for QuitEventWatcher {}

impl QuitEventWatcher {
    fn start(home: &Path, on_quit: impl FnOnce() + Send + 'static) -> Option<Self> {
        let name = PipeNames::agent_quit_event_for(home);
        let name_w = to_wide_z(&name);
        let quit_ev = unsafe { CreateEventW(None, false, false, PCWSTR(name_w.as_ptr())) }.ok()?;
        let stop_ev = unsafe { CreateEventW(None, true, false, None) }.ok()?;
        let q_raw = quit_ev.0 as isize;
        let s_raw = stop_ev.0 as isize;
        let thread = thread::Builder::new()
            .name("BetterRecall.QuitEvent".to_string())
            .spawn(move || {
                let handles = [
                    HANDLE(q_raw as *mut std::ffi::c_void),
                    HANDLE(s_raw as *mut std::ffi::c_void),
                ];
                let wait = unsafe { WaitForMultipleObjects(&handles, false, INFINITE) };
                unsafe {
                    let _ = CloseHandle(handles[0]);
                }
                if wait == WAIT_OBJECT_0 {
                    AgentLog::info("quit event set");
                    on_quit();
                }
            })
            .ok()?;
        Some(Self {
            stop_event: stop_ev,
            thread: Some(thread),
        })
    }

    fn stop(&mut self) {
        if !self.stop_event.is_invalid() && !self.stop_event.0.is_null() {
            unsafe {
                let _ = SetEvent(self.stop_event);
            }
            if let Some(h) = self.thread.take() {
                let _ = h.join();
            }
            unsafe {
                let _ = CloseHandle(self.stop_event);
            }
            self.stop_event = HANDLE::default();
        }
    }
}

impl Drop for QuitEventWatcher {
    fn drop(&mut self) {
        self.stop();
    }
}

struct StopWritersState {
    started: bool,
    done: bool,
}

/// Composition root and `AgentControl`: wires `CoreHost`, worker supervision, embedding scheduler,
/// guardrails, tray/message window, hotkeys, read gate, pause controller, and A5/A6 seams.
pub struct AgentHost {
    core: Arc<CoreHost>,
    opt: AgentOptions,
    gate: Arc<ReadGate>,
    pause: Arc<PauseController>,
    supervisor: Arc<WorkerSupervisor>,
    worker: Arc<WorkerClient>,
    thumbs: Arc<dyn Thumbnailer>,
    capture: Arc<dyn CaptureSubsystem>,
    keyframes: Arc<dyn KeyframeSubsystem>,
    replays: Arc<dyn ReplaySubsystem>,
    audio: Arc<dyn AudioSubsystem>,
    cts: CancelSource,
    window: Mutex<Option<Arc<MessageWindow>>>,
    quit_watcher: Mutex<Option<QuitEventWatcher>>,
    guard: Mutex<Option<Arc<GuardrailService>>>,
    scheduler: Mutex<Option<Arc<EmbeddingScheduler>>>,
    probe: Mutex<Option<Arc<Mutex<SystemProbe>>>>,
    storage: Mutex<Option<Arc<StorageGuard>>>,
    storage_gate: Mutex<()>,
    session_locked: AtomicBool,
    capture_started: AtomicBool,
    late_isolation_done: AtomicBool,
    retention_skip_logged: AtomicBool,
    hotkeys_failed: Mutex<Option<Vec<String>>>,
    last_status_key: Mutex<String>,
    stats_cache: Mutex<Option<(StoreStats, i64)>>,
    settings_snapshot: Mutex<Settings>,
    status_listeners: Arc<Mutex<Vec<(u64, StatusListener)>>>,
    capture_listeners: Arc<Mutex<Vec<(u64, CaptureListener)>>>,
    panic_listeners: Arc<Mutex<Vec<(u64, PanicListener)>>>,
    next_listener_id: AtomicU64,
    timer_stop: Arc<(Mutex<bool>, Condvar)>,
    timers: Mutex<Vec<JoinHandle<()>>>,
    stop_writers: Mutex<StopWritersState>,
    stop_writers_cv: Condvar,
    settings_guard: Mutex<Option<ListenerGuard>>,
    purged_guard: Mutex<Option<ListenerGuard>>,
}

impl AgentHost {
    /// Slack on top of `replay.seconds` when deciding whether a purge reaches the replay ring.
    pub const RING_SLACK_SECS: i64 = 60;

    pub fn new(core: Arc<CoreHost>, opt: AgentOptions) -> Arc<Self> {
        let initial_settings = core.settings().current();
        let worker_exe = InstallLayout::full(&opt.install_dir, InstallLayout::WORKER_EXE);
        VcRuntime::set_worker_dir(worker_exe.parent().map(PathBuf::from));

        let settings_store = core.settings().clone();
        let settings_for_eco = settings_store.clone();
        let settings_for_json = settings_store.clone();
        let launcher = Arc::new(ProcessWorkerLauncher::new(
            worker_exe,
            move || settings_for_eco.current().governance.eco_qos,
            Some(Arc::new(move || {
                WorkerSettingsDiff::json(&settings_for_json.current())
            })),
        ));
        let settings_for_idle = settings_store.clone();
        let supervisor = WorkerSupervisor::new(launcher, opt.home.clone(), move || {
            settings_for_idle.current().inference.idle_exit_s
        });
        let worker = Arc::new(WorkerClient::new(supervisor.clone()));

        Arc::new(Self {
            core,
            opt,
            gate: Arc::new(ReadGate::new()),
            pause: Arc::new(PauseController::new()),
            supervisor,
            worker,
            thumbs: Arc::new(UnavailableThumbnailer),
            capture: Arc::new(UnavailableCapture),
            keyframes: Arc::new(UnavailableKeyframes),
            replays: Arc::new(UnavailableReplay),
            audio: Arc::new(UnavailableAudio),
            cts: CancelSource::new(),
            window: Mutex::new(None),
            quit_watcher: Mutex::new(None),
            guard: Mutex::new(None),
            scheduler: Mutex::new(None),
            probe: Mutex::new(None),
            storage: Mutex::new(None),
            storage_gate: Mutex::new(()),
            session_locked: AtomicBool::new(false),
            capture_started: AtomicBool::new(false),
            late_isolation_done: AtomicBool::new(false),
            retention_skip_logged: AtomicBool::new(false),
            hotkeys_failed: Mutex::new(None),
            last_status_key: Mutex::new(String::new()),
            stats_cache: Mutex::new(None),
            settings_snapshot: Mutex::new(initial_settings),
            status_listeners: Arc::new(Mutex::new(Vec::new())),
            capture_listeners: Arc::new(Mutex::new(Vec::new())),
            panic_listeners: Arc::new(Mutex::new(Vec::new())),
            next_listener_id: AtomicU64::new(1),
            timer_stop: Arc::new((Mutex::new(false), Condvar::new())),
            timers: Mutex::new(Vec::new()),
            stop_writers: Mutex::new(StopWritersState {
                started: false,
                done: false,
            }),
            stop_writers_cv: Condvar::new(),
            settings_guard: Mutex::new(None),
            purged_guard: Mutex::new(None),
        })
    }

    fn settings(&self) -> Settings {
        self.settings_snapshot
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn store(&self) -> Option<Arc<EncryptedStore>> {
        self.core.store()
    }

    pub fn supervisor(&self) -> &Arc<WorkerSupervisor> {
        &self.supervisor
    }

    pub fn scheduler(&self) -> Option<Arc<EmbeddingScheduler>> {
        self.scheduler
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn guard(&self) -> Option<Arc<GuardrailService>> {
        self.guard
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn capture_diagnostics(&self) -> Option<String> {
        self.capture.diagnostics()
    }

    pub fn keyframe_diagnostics(&self) -> Option<String> {
        self.keyframes.diagnostics()
    }

    pub fn request_quit(&self) {
        if let Some(w) = self
            .window
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
        {
            w.quit();
        }
    }

    pub fn set_capture_started_for_test(&self, started: bool) {
        self.capture_started.store(started, Ordering::Release);
    }

    // ---------------- startup ----------------

    pub fn run(self: &Arc<Self>) -> Result<(), String> {
        let s = self.settings();
        AgentLog::set_level(&s.log.level);
        if s.security.memory_isolation {
            MemoryIsolation::apply_early(&self.opt.install_dir);
        }
        if s.governance.eco_qos {
            unsafe {
                Governance::apply_eco_qos(GetCurrentProcess(), false);
            }
        }

        let window = MessageWindow::new()?;
        *self.window.lock().unwrap_or_else(|e| e.into_inner()) = Some(window.clone());

        let me_quit = Arc::downgrade(self);
        if let Some(watcher) = QuitEventWatcher::start(&self.opt.home, move || {
            if let Some(h) = me_quit.upgrade() {
                h.request_quit();
            }
        }) {
            *self.quit_watcher.lock().unwrap_or_else(|e| e.into_inner()) = Some(watcher);
        }

        {
            let me = Arc::downgrade(self);
            window.set_on_hotkey(Arc::new(move |name| {
                if let Some(h) = me.upgrade() {
                    h.on_hotkey(name);
                }
            }));
        }
        {
            let me = Arc::downgrade(self);
            window.set_on_command(Arc::new(move |cmd| {
                if let Some(h) = me.upgrade() {
                    h.on_tray_command(cmd);
                }
            }));
        }
        {
            let install_dir = self.opt.install_dir.clone();
            window.set_on_balloon_clicked(Arc::new(move || {
                UiLauncher::start(&install_dir, &["--replays"]);
            }));
        }
        {
            let me = Arc::downgrade(self);
            window.set_on_session_changed(Arc::new(move |code| {
                if let Some(h) = me.upgrade() {
                    h.on_session_changed(code);
                }
            }));
        }
        {
            let cap = self.capture.clone();
            window.set_on_display_changed(Arc::new(move || cap.rebuild()));
        }
        {
            let me = Arc::downgrade(self);
            window.set_on_end_session(Arc::new(move || {
                if let Some(h) = me.upgrade() {
                    h.on_end_session();
                }
            }));
        }
        {
            let me = Arc::downgrade(self);
            window.set_menu_state(Arc::new(move || {
                if let Some(h) = me.upgrade() {
                    let (reason, _) = h.pause.current();
                    MenuStateSnapshot {
                        status: h.status_line(),
                        paused: matches!(reason.as_deref(), Some("user" | "ipc")),
                        replay_on: h.capture.replay_active(),
                    }
                } else {
                    MenuStateSnapshot {
                        status: "BetterRecall".to_string(),
                        paused: false,
                        replay_on: false,
                    }
                }
            }));
        }

        window.show_tray("BetterRecall");
        self.register_hotkeys();

        {
            let me = Arc::downgrade(self);
            let guard = self.core.settings().on_changed_guarded(move |next| {
                if let Some(h) = me.upgrade() {
                    h.on_settings_changed(next.clone());
                }
            });
            *self
                .settings_guard
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = Some(guard);
        }

        self.replays.delete_partials();

        let probe = Arc::new(Mutex::new(SystemProbe::new(
            HardwareProbe::intel_igpu_pdh_token(),
        )));
        *self.probe.lock().unwrap_or_else(|e| e.into_inner()) = Some(probe.clone());

        let core_for_sched = self.core.clone();
        let me_for_sched_s = Arc::downgrade(self);
        let pause_for_sched = self.pause.clone();
        let me_for_dim = Arc::downgrade(self);
        let probe_for_sched = probe.clone();
        let scheduler = EmbeddingScheduler::new(
            Arc::new(move || {
                core_for_sched
                    .store()
                    .map(|st| st as Arc<dyn EmbeddingStore>)
            }),
            Arc::new(move || {
                me_for_sched_s
                    .upgrade()
                    .map(|h| h.settings())
                    .unwrap_or_default()
            }),
            self.worker.clone(),
            Arc::new(move || {
                let mut p = probe_for_sched.lock().unwrap_or_else(|e| e.into_inner());
                GovernanceInputs {
                    cpu_percent: p.cpu_percent(),
                    igpu_percent: p.igpu_percent(),
                    on_battery: SystemProbe::on_battery(),
                    notification: SystemProbe::notification(),
                    user_idle_ms: crate::native::idle_milliseconds(),
                }
            }),
            Arc::new(move || {
                pause_for_sched.has("consent")
                    || pause_for_sched.has("keylocked")
                    || pause_for_sched.has("keylost")
                    || pause_for_sched.has("storeerror")
            }),
            Some(Arc::new(move || {
                me_for_dim
                    .upgrade()
                    .map(|h| h.settings().index.dim)
                    .unwrap_or(768)
            })),
        );
        *self.scheduler.lock().unwrap_or_else(|e| e.into_inner()) = Some(scheduler.clone());

        let me_for_guard_s = Arc::downgrade(self);
        let me_for_guard_lock = Arc::downgrade(self);
        let cap_for_guard = self.capture.clone();
        let guard = GuardrailService::new(
            move || {
                me_for_guard_s
                    .upgrade()
                    .map(|h| h.settings())
                    .unwrap_or_default()
            },
            move || {
                me_for_guard_lock
                    .upgrade()
                    .is_some_and(|h| h.session_locked.load(Ordering::Acquire))
            },
            move || cap_for_guard.monitor_bounds(),
            &self.opt.install_dir,
        );
        {
            let cap = self.capture.clone();
            guard.on_foreground_changed(Arc::new(move |_| cap.on_foreground_changed()));
        }
        {
            let g_weak = Arc::downgrade(&guard);
            window.set_on_foreground_changed(Arc::new(move || {
                if let Some(g) = g_weak.upgrade() {
                    g.on_foreground_event();
                }
            }));
        }
        *self.guard.lock().unwrap_or_else(|e| e.into_inner()) = Some(guard.clone());

        {
            let me = Arc::downgrade(self);
            let pg = self.core.on_purged(move |n| {
                if let Some(h) = me.upgrade() {
                    h.on_purged(n);
                }
            });
            *self.purged_guard.lock().unwrap_or_else(|e| e.into_inner()) = Some(pg);
        }

        let embedder = Arc::new(QueryEmbedderImpl::new(self.worker.clone()));
        self.core
            .start_pipes(self.clone(), embedder, self.thumbs.clone());

        if !s.capture.consented {
            self.pause.set("consent", None);
        }
        self.pause.set("keylocked", None);

        {
            let me = self.clone();
            let _ = thread::Builder::new()
                .name("BetterRecall.OpenStore".to_string())
                .spawn(move || me.open_store_loop());
        }

        {
            let cap = self.capture.clone();
            self.pause.on_changed(move || cap.apply_settings());
        }
        self.pause.on_expired(|r| {
            if r == "user" || r == "ipc" {
                AgentLog::info(&format!("resumed ({r} pause ended)"));
            }
        });

        if session_locked_now() == Some(true) {
            AgentLog::info("session is locked at start");
            self.on_session_changed(WTS_SESSION_LOCK);
        }

        guard.start();
        scheduler.start();

        let core_for_storage = self.core.clone();
        let me_for_storage_s = Arc::downgrade(self);
        let home_for_storage = self.opt.home.clone();
        let storage = Arc::new(StorageGuard::new(
            move || {
                core_for_storage
                    .store()
                    .map(|st| st as Arc<dyn StorageStore>)
            },
            move || {
                me_for_storage_s
                    .upgrade()
                    .map(|h| h.settings())
                    .unwrap_or_default()
            },
            move || StorageGuard::free_bytes_of(&home_for_storage),
        ));
        *self.storage.lock().unwrap_or_else(|e| e.into_inner()) = Some(storage);

        self.spawn_periodic_timers();

        if self.opt.autostart && StartupRegistration::is_installed(&self.opt.install_dir, &self.opt.home)
            && let Err(e) = StartupRegistration::register(&self.opt.install_dir) {
                AgentLog::warn_err("startup registration", &e);
            }
        if s.capture.consented {
            self.start_capture();
        }
        if !s.ui.start_minimized {
            UiLauncher::start(&self.opt.install_dir, &["--page", "home"]);
        }

        // Diagnostics CPU mark after 15 s warm-up.
        {
            let stop_pair = self.timer_stop.clone();
            if let Ok(h) = thread::Builder::new()
                .name("BetterRecall.DiagMark".to_string())
                .spawn(move || {
                    let (lock, cv) = &*stop_pair;
                    let g = lock.lock().unwrap_or_else(|e| e.into_inner());
                    if *g {
                        return;
                    }
                    let (g2, _) = cv
                        .wait_timeout(g, Duration::from_secs(15))
                        .unwrap_or_else(|e| e.into_inner());
                    if !*g2 {
                        Diagnostics::mark();
                    }
                })
            {
                self.timers.lock().unwrap_or_else(|e| e.into_inner()).push(h);
            }
        }

        AgentLog::info("agent running");
        window.run();
        AgentLog::info("message loop ended");
        Ok(())
    }

    fn spawn_periodic_timers(self: &Arc<Self>) {
        let mut list = self.timers.lock().unwrap_or_else(|e| e.into_inner());
        // Status tick: 1 s
        list.push(self.spawn_timer(
            "BetterRecall.StatusTimer",
            Duration::from_secs(1),
            Duration::from_secs(1),
            |h| h.status_tick(),
        ));
        // Maintenance: 1 h
        list.push(self.spawn_timer(
            "BetterRecall.MaintenanceTimer",
            Duration::from_secs(3600),
            Duration::from_secs(3600),
            |h| h.maintenance(),
        ));
        // Storage check: 5 s initial, 30 s period
        list.push(self.spawn_timer(
            "BetterRecall.StorageTimer",
            Duration::from_secs(5),
            Duration::from_secs(30),
            |h| h.storage_check(),
        ));
    }

    fn spawn_timer(
        self: &Arc<Self>,
        name: &str,
        initial: Duration,
        period: Duration,
        tick: fn(&Arc<Self>),
    ) -> JoinHandle<()> {
        let weak = Arc::downgrade(self);
        let stop_pair = self.timer_stop.clone();
        thread::Builder::new()
            .name(name.to_string())
            .spawn(move || {
                let mut wait = initial;
                loop {
                    let (lock, cv) = &*stop_pair;
                    let g = lock.lock().unwrap_or_else(|e| e.into_inner());
                    if *g {
                        return;
                    }
                    let (g2, _) = cv
                        .wait_timeout(g, wait)
                        .unwrap_or_else(|e| e.into_inner());
                    if *g2 {
                        return;
                    }
                    drop(g2);
                    let Some(h) = weak.upgrade() else { return };
                    tick(&h);
                    wait = period;
                }
            })
            .expect("spawn timer thread")
    }

    fn open_store_loop(self: &Arc<Self>) {
        let ct = self.cts.token();
        loop {
            if ct.is_cancelled() {
                return;
            }
            let sw = Instant::now();
            match self.core.open_store(&ct) {
                Ok(()) => {
                    AgentLog::info(&format!(
                        "store open in {} ms ({})",
                        sw.elapsed().as_millis(),
                        self.core.keys().protection()
                    ));
                    self.pause.clear("keylocked");
                    self.pause.clear("storeerror");
                    // D5: prewarm worker so first search does not pay 10-15 s cold start.
                    let worker = self.worker.clone();
                    let _ = thread::Builder::new()
                        .name("BetterRecall.Prewarm".to_string())
                        .spawn(move || {
                            let _ = worker.prewarm(&CancelToken::none());
                        });
                    self.maintenance();
                    if let Some(sc) = self.scheduler() {
                        sc.signal();
                    }
                    self.raise_status(true);
                    return;
                }
                Err(StoreError::Cancelled) => return,
                Err(StoreError::KeyLost(msg)) => {
                    AgentLog::error_err("store open failed", &msg);
                    self.pause.set("keylost", None);
                    self.pause.clear("keylocked");
                    self.raise_status(true);
                    return;
                }
                Err(e) => {
                    AgentLog::error_err("store open failed", &e.to_string());
                    self.pause.set("storeerror", None);
                    self.pause.clear("keylocked");
                    self.raise_status(true);
                    let (lock, cv) = &*self.timer_stop;
                    let g = lock.lock().unwrap_or_else(|e| e.into_inner());
                    if *g || ct.is_cancelled() {
                        return;
                    }
                    let _ = cv.wait_timeout(g, Duration::from_secs(30));
                }
            }
        }
    }

    fn start_capture(self: &Arc<Self>) {
        if self.capture_started.swap(true, Ordering::AcqRel) {
            return;
        }
        let s = self.settings();
        if s.security.memory_isolation {
            let ring = (s.replay.bitrate_kbps as i64) * 1000 / 8
                * (s.replay.seconds as i64 + 2)
                * 3
                + 4 * 1024 * 1024;
            MemoryIsolation::reserve_working_set(ring);
        }
        self.capture.start();
        self.audio.sync_sources(&s, true);

        let me = Arc::downgrade(self);
        let stop_pair = self.timer_stop.clone();
        let _ = thread::Builder::new()
            .name("BetterRecall.LateIsolation".to_string())
            .spawn(move || {
                let (lock, cv) = &*stop_pair;
                let g = lock.lock().unwrap_or_else(|e| e.into_inner());
                if *g {
                    return;
                }
                let (g2, _) = cv
                    .wait_timeout(g, Duration::from_secs(8))
                    .unwrap_or_else(|e| e.into_inner());
                if *g2 {
                    return;
                }
                drop(g2);
                let Some(h) = me.upgrade() else { return };
                if h.settings().security.memory_isolation
                    && !h.late_isolation_done.swap(true, Ordering::AcqRel)
                {
                    if let Err(e) = EncryptedStore::warm_native_callbacks() {
                        AgentLog::warn_err("sqlite callback warm-up", &e.to_string());
                    }
                    let acg = std::env::var("BETTERRECALL_NO_ACG").as_deref() != Ok("1");
                    MemoryIsolation::apply_late(acg);
                }
            });
    }

    /// Pure replay pause decision (`AgentHost.ShouldPauseReplay`).
    pub fn should_pause_replay(
        pause: &PauseController,
        during_fullscreen: bool,
        store_open: bool,
        consented: bool,
    ) -> bool {
        pause.continuous_paused(during_fullscreen) || !store_open || !consented
    }

    /// Pure audio sync decision (`AgentHost.AudioNeedsSync`).
    pub fn audio_needs_sync(
        want_system: bool,
        system_open: bool,
        want_mic: bool,
        mic_open: bool,
        mic_lost: bool,
    ) -> bool {
        mic_lost || (want_mic && !mic_open) || (want_system && !system_open)
    }

    /// Pure decision: `ocr.enabled` or a `privacy.redact*` text detector went from off to on.
    pub fn text_detection_turned_on(old: &Settings, s: &Settings) -> bool {
        let any_text = |p: &PrivacySettings| {
            p.redact_passwords
                || p.redact_cards
                || p.redact_ids
                || p.redact_birth_dates
                || p.redact_emails
                || p.redact_phones
                || p.redact_names
                || p.redact_addresses
        };
        if !s.ocr.enabled || !any_text(&s.privacy) {
            return false;
        }
        let a = &old.privacy;
        let b = &s.privacy;
        !old.ocr.enabled
            || (b.redact_passwords && !a.redact_passwords)
            || (b.redact_cards && !a.redact_cards)
            || (b.redact_ids && !a.redact_ids)
            || (b.redact_birth_dates && !a.redact_birth_dates)
            || (b.redact_emails && !a.redact_emails)
            || (b.redact_phones && !a.redact_phones)
            || (b.redact_names && !a.redact_names)
            || (b.redact_addresses && !a.redact_addresses)
    }

    fn register_hotkeys(&self) {
        let Some(win) = self
            .window
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
        else {
            return;
        };
        let s = self.settings();
        let h = &s.hotkeys;
        let all = [
            (HK_SEARCH, "search", h.search.as_str()),
            (HK_PAUSE, "pause", h.pause.as_str()),
            (HK_REPLAY, "replay", h.replay.as_str()),
            (HK_PANIC, "panic", h.panic.as_str()),
            (HK_TIMELINE, "timeline", h.timeline.as_str()),
            (HK_HOME, "home", h.home.as_str()),
            (HK_LOCK, "lock", h.lock.as_str()),
        ];
        for (id, _, _) in all {
            win.unregister_hotkey(id);
        }
        let mut failed = Vec::new();
        for (id, name, text) in all {
            if text.trim().is_empty() {
                continue;
            }
            let Some(hk) = HotkeyParser::parse(Some(text)) else {
                AgentLog::warn(&format!("hotkey {name} invalid"));
                failed.push(name.to_string());
                continue;
            };
            if !win.register_hotkey(id, name, hk) {
                failed.push(name.to_string());
            }
        }
        *self
            .hotkeys_failed
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = if failed.is_empty() {
            None
        } else {
            Some(failed)
        };
    }

    // ---------------- events ----------------

    fn on_settings_changed(self: &Arc<Self>, s: Settings) {
        let old = {
            let mut snap = self
                .settings_snapshot
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            std::mem::replace(&mut *snap, s.clone())
        };
        AgentLog::set_level(&s.log.level);

        if let Some(win) = self
            .window
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
        {
            let me = self.clone();
            let old_hk = old.hotkeys.clone();
            let old_consented = old.capture.consented;
            let old_monitors = old.capture.monitors.clone();
            let next = s.clone();
            win.invoke(move || {
                if !hotkeys_equal(&old_hk, &next.hotkeys) {
                    me.register_hotkeys();
                }
                if next.capture.consented && !old_consented {
                    me.pause.clear("consent");
                    me.start_capture();
                }
                if old_monitors != next.capture.monitors {
                    me.capture.rebuild();
                } else {
                    me.capture.apply_settings();
                }
                me.audio
                    .sync_sources(&next, me.capture_started.load(Ordering::Acquire));
                me.raise_status(true);
            });
        }

        if !storage_equal(&old.storage, &s.storage) {
            let me = self.clone();
            let _ = thread::Builder::new()
                .name("BetterRecall.StorageCheck".to_string())
                .spawn(move || me.storage_check());
        }
        if Self::text_detection_turned_on(&old, &s) {
            self.keyframes.mark_all_unchecked();
            self.capture.retry_all();
        }
        if let Some(key) = WorkerSettingsDiff::changed(&old, &s) {
            self.supervisor.request_restart(key);
        }
    }

    fn storage_check(&self) {
        let Ok(_gate) = self.storage_gate.try_lock() else {
            return;
        };
        let Some(sg) = self
            .storage
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
        else {
            return;
        };
        if self.store().is_none() {
            return;
        }
        let d = sg.check();
        if d.deleted > 0 {
            *self.stats_cache.lock().unwrap_or_else(|e| e.into_inner()) = None;
            AgentLog::info(&format!(
                "storage: {} captures deleted to make room",
                d.deleted
            ));
        }
        if d.pause && !self.pause.has("storage") {
            self.pause.set("storage", None);
            AgentLog::warn(&format!("capture paused: storage ({})", d.why));
        } else if !d.pause && self.pause.has("storage") {
            self.pause.clear("storage");
            AgentLog::info("capture resumed: storage back within limits");
        }
        if d.notify
            && let Some(win) = self
                .window
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
        {
            let st = self.settings().storage;
            let body = if d.why == "disk" {
                format!(
                    "Less than {} GB free on the data drive. Capture is paused until there is room.",
                    st.min_free_gb
                )
            } else {
                format!(
                    "Stored captures reached {} GB. Capture is paused; raise the limit or turn on overwrite.",
                    st.max_gb
                )
            };
            win.notify("BetterRecall paused", &body);
        }
        self.raise_status(false);
    }

    fn on_hotkey(self: &Arc<Self>, name: &str) {
        AgentLog::info(&format!("hotkey {name}"));
        match name {
            "search" => {
                let w = self.worker.clone();
                let _ = thread::Builder::new()
                    .name("BetterRecall.Prewarm".to_string())
                    .spawn(move || {
                        let _ = w.prewarm(&CancelToken::none());
                    });
                UiLauncher::show_launcher(&self.opt.install_dir, &self.opt.home);
            }
            "pause" => {
                if self.pause.has("user") || self.pause.has("ipc") {
                    self.resume("user");
                } else {
                    self.pause(None, "user");
                }
            }
            "replay" => self.save_replay_from_hotkey(),
            "panic" => self.panic_off_loop(),
            "timeline" => {
                UiLauncher::start(&self.opt.install_dir, &["--page", "timeline"]);
            }
            "home" => {
                UiLauncher::start(&self.opt.install_dir, &["--page", "home"]);
            }
            "lock" => self.lock(),
            _ => {}
        }
    }

    fn save_replay_from_hotkey(self: &Arc<Self>) {
        let me = self.clone();
        let _ = thread::Builder::new()
            .name("BetterRecall.SaveReplay".to_string())
            .spawn(move || {
                if let Err(e) = me.save_replay(&CancelToken::none()) {
                    AgentLog::warn_err("replay save", &e.to_string());
                    if let Some(win) = me.window.lock().unwrap_or_else(|e| e.into_inner()).clone() {
                        win.notify("BetterRecall", "Replay could not be saved.");
                    }
                }
            });
    }

    fn panic_off_loop(self: &Arc<Self>) {
        let me = self.clone();
        let _ = thread::Builder::new()
            .name("BetterRecall.Panic".to_string())
            .spawn(move || me.panic());
    }

    fn on_tray_command(self: &Arc<Self>, cmd: TrayCommand) {
        match cmd {
            TrayCommand::Search => self.on_hotkey("search"),
            TrayCommand::Pause15 => self.pause(Some(Duration::from_secs(15 * 60)), "user"),
            TrayCommand::Pause60 => self.pause(Some(Duration::from_secs(60 * 60)), "user"),
            TrayCommand::PauseUntilResume => self.pause(None, "user"),
            TrayCommand::Resume => self.resume("user"),
            TrayCommand::SaveReplay => self.save_replay_from_hotkey(),
            TrayCommand::Panic => self.panic_off_loop(),
            TrayCommand::Settings => {
                UiLauncher::start(&self.opt.install_dir, &["--settings"]);
            }
            TrayCommand::Quit => self.request_quit(),
            TrayCommand::Status => {}
        }
    }

    pub fn on_session_changed(&self, code: i32) {
        let s = self.settings();
        match code {
            WTS_SESSION_LOCK | WTS_CONSOLE_DISCONNECT | WTS_REMOTE_DISCONNECT => {
                if s.privacy.pause_on_lock {
                    self.pause.set("lock", None);
                }
                self.audio.flush();
                self.session_locked.store(true, Ordering::Release);
                if s.security.lock_on_session_lock {
                    self.gate.close();
                }
                self.replays.clear_sampler();
            }
            WTS_SESSION_UNLOCK | WTS_CONSOLE_CONNECT | WTS_REMOTE_CONNECT => {
                self.session_locked.store(false, Ordering::Release);
                self.pause.clear("lock");
            }
            _ => {}
        }
        if let Some(g) = self.guard() {
            g.refresh();
        }
        self.raise_status(true);
    }

    /// Pure decision: whether a purge reaches the live replay buffers (`AgentHost.ReachesRing`).
    pub fn reaches_ring(n: &PurgeNotice, now: DateTime<Utc>, replay_seconds: i32) -> bool {
        if !n.by_user() {
            return false;
        }
        if n.reason == "all" || matches!(n.scope.as_deref(), Some("app" | "site")) {
            return true;
        }
        let span = replay_seconds.clamp(15, 600) as i64 + Self::RING_SLACK_SECS;
        let edge = now - ChronoDuration::seconds(span);
        n.to.is_some_and(|to| to >= edge) || n.newest.is_some_and(|newest| newest >= edge)
    }

    pub fn on_purged(&self, n: &PurgeNotice) {
        self.thumbs.clear_cache();
        if n.reason == "all" {
            if let Some(sc) = self.scheduler() {
                sc.clear();
            }
            self.replays.on_purge_all();
            if self.settings().security.key_protection == KeyVault::MODE_PASSPHRASE {
                self.pause.set("keylocked", None);
                self.raise_status(true);
            }
        } else if let Some(sc) = self.scheduler() {
            let ids: HashSet<String> = n.ids.iter().cloned().collect();
            let _ = sc.remove(&ids);
        }
        *self.stats_cache.lock().unwrap_or_else(|e| e.into_inner()) = None;
        if Self::reaches_ring(n, Utc::now(), self.settings().replay.seconds) {
            self.capture.forget_recent();
            self.replays.clear_sampler();
            self.audio.clear();
            self.replays.discard_saves_in_flight();
            AgentLog::info(&format!(
                "purge ({}) reached the replay window: replay ring, samples and pending audio cleared",
                n.reason
            ));
        }
    }

    fn status_tick(self: &Arc<Self>) {
        let s = self.settings();
        let n = SystemProbe::notification();
        if Governance::pause_keyframes(n, &s.capture) {
            if !self.pause.has("fullscreen") {
                self.pause.set("fullscreen", None);
            }
        } else {
            self.pause.clear("fullscreen");
        }
        let cap_started = self.capture_started.load(Ordering::Acquire);
        if cap_started && self.audio.needs_sync(&s, cap_started) {
            self.audio.sync_sources(&s, cap_started);
        }
        self.audio.check_health(tick_count64());

        let store_open = self.store().is_some();
        Self::sync_key_lock_pause(&self.pause, store_open, self.core.keys().needs_passphrase());
        if (self.pause.has("keylost")
            || self.pause.has("storeerror")
            || self.pause.has("keylocked"))
            && store_open
        {
            self.pause.clear("keylost");
            self.pause.clear("storeerror");
            self.pause.clear("keylocked");
            AgentLog::info("store open after purge all");
            self.maintenance();
            if let Some(sc) = self.scheduler() {
                sc.signal();
            }
        }
        self.raise_status(false);
        if let Some(win) = self
            .window
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
        {
            let tip = format!("BetterRecall: {}", self.status_line());
            let win_cb = win.clone();
            win.invoke(move || win_cb.update_tip(&tip));
        }
    }

    pub fn sync_key_lock_pause(
        pause: &PauseController,
        store_open: bool,
        needs_passphrase: bool,
    ) {
        if !store_open && needs_passphrase {
            pause.set("keylocked", None);
        }
    }

    fn status_line(&self) -> String {
        let (reason, until) = self.pause.current();
        let s = self.settings();
        let Some(reason) = reason else {
            if !self.capture_started.load(Ordering::Acquire) {
                return "Starting".to_string();
            }
            if !VcRuntime::present() {
                return "Capturing (search off: install the Microsoft Visual C++ 2015-2022 Redistributable, x64)".to_string();
            }
            if s.ocr.enabled && self.keyframes.ocr_failing() {
                let any_text = s.privacy.redact_passwords
                    || s.privacy.redact_cards
                    || s.privacy.redact_ids
                    || s.privacy.redact_birth_dates
                    || s.privacy.redact_emails
                    || s.privacy.redact_phones
                    || s.privacy.redact_names
                    || s.privacy.redact_addresses;
                return if any_text {
                    "Capturing (keyframes dropped: text recognition failing)".to_string()
                } else {
                    "Capturing (keyframes without text: text recognition failing)".to_string()
                };
            }
            if s.replay.enabled
                && let Some(err) = self.capture.replay_last_error()
                && err.starts_with("encoder failed")
            {
                return format!("Capturing (replay {err})");
            }
            if let Some(budget) = self.keyframes.budget_status() {
                return budget;
            }
            return "Capturing".to_string();
        };

        match reason.as_str() {
            "consent" if self.core.settings().load_error().is_some() => {
                "Waiting for consent (settings.json unreadable, defaults in use until you save)"
                    .to_string()
            }
            "consent" => "Waiting for consent".to_string(),
            "keylocked" => "Locked: enter passphrase".to_string(),
            "keylost" => "Data key missing: purge all in Danger zone to start over".to_string(),
            "storeerror" => "Paused: storage could not be opened, retrying".to_string(),
            "lock" => "Paused: session locked".to_string(),
            "fullscreen" => "Paused: fullscreen".to_string(),
            "storage" => "Paused: storage limit".to_string(),
            _ => match until {
                Some(u) => {
                    let local: DateTime<Local> = u.with_timezone(&Local);
                    format!("Paused until {}", local.format("%H:%M"))
                }
                None => "Paused".to_string(),
            },
        }
    }

    fn raise_status(&self, force: bool) {
        let st = self.get_status();
        let warnings_str = st
            .warnings
            .as_ref()
            .map(|w| w.join(","))
            .unwrap_or_default();
        let key = format!(
            "{}|{}|{:?}|{:?}|{}|{}|{}|{:?}|{}|{warnings_str}",
            st.capturing,
            st.paused,
            st.pause_reason,
            st.paused_until,
            st.locked,
            st.device,
            st.queue.pending,
            st.audio,
            st.user_paused
        );
        {
            let mut last = self
                .last_status_key
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if !force && *last == key {
                return;
            }
            *last = key;
        }
        let listeners: Vec<StatusListener> = self
            .status_listeners
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|(_, cb)| cb.clone())
            .collect();
        for cb in listeners {
            if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| cb(&st))).is_err() {
                AgentLog::warn("StatusChanged handler panicked");
            }
        }
    }

    fn maintenance(&self) {
        let Some(store) = self.store() else { return };
        let sw = Instant::now();
        let mut skip_logged = self.retention_skip_logged.load(Ordering::Acquire);
        let allowed = Self::retention_allowed(Some(self.core.settings()), &mut skip_logged);
        self.retention_skip_logged
            .store(skip_logged, Ordering::Release);
        match store.run_maintenance(&self.settings(), allowed) {
            Ok(()) => {
                AgentLog::info(&format!(
                    "maintenance done in {} ms",
                    sw.elapsed().as_millis()
                ));
                self.storage_check();
            }
            Err(e) => AgentLog::warn_err("maintenance", &e.to_string()),
        }
    }

    /// Retention and thinning wait while `settings.json` is unreadable (S11).
    pub fn retention_allowed(settings: Option<&SettingsStore>, skip_logged: &mut bool) -> bool {
        let ok = settings.is_none_or(|s| s.load_error().is_none());
        if !ok && !*skip_logged {
            AgentLog::warn(
                "maintenance: settings.json is unreadable; retention and thinning skipped until it is reset or fixed",
            );
        } else if ok && *skip_logged {
            AgentLog::info(
                "maintenance: settings.json readable again; retention and thinning resume",
            );
        }
        *skip_logged = !ok;
        ok
    }

    fn warnings(&self) -> Option<Vec<String>> {
        let mut list = Vec::new();
        if self.core.settings().load_error().is_some() {
            list.push("settings-unreadable".to_string());
        }
        if !VcRuntime::present() {
            list.push("vc-runtime-missing".to_string());
        }
        if self.capture.replay_text_unchecked() {
            list.push("replay-text-unchecked".to_string());
        }
        if self.settings().ocr.enabled {
            if self.keyframes.ocr_failing() {
                list.push("ocr-failing".to_string());
            }
            if self.keyframes.entities_missing() {
                list.push("ner-unavailable".to_string());
            }
        }
        if list.is_empty() { None } else { Some(list) }
    }

    fn store_stats(&self) -> StoreStats {
        let now = tick_count64();
        {
            let cache = self.stats_cache.lock().unwrap_or_else(|e| e.into_inner());
            if let Some((ref st, at)) = *cache
                && now.saturating_sub(at) < 5000
            {
                return st.clone();
            }
        }
        let st = self
            .store()
            .and_then(|s| s.stats().ok())
            .unwrap_or_default();
        *self.stats_cache.lock().unwrap_or_else(|e| e.into_inner()) = Some((st.clone(), now));
        st
    }

    pub fn clear_for_resume(pause: &PauseController, reason: &str) {
        if reason != "ipc" {
            pause.clear("user");
        }
        pause.clear("ipc");
    }

    // ---------------- shutdown ----------------

    /// Stops pipes and capture/keyframe/scheduler writers concurrently once (C4).
    pub fn stop_session_writers(&self) {
        let should_run = {
            let mut st = self.stop_writers.lock().unwrap_or_else(|e| e.into_inner());
            if st.started {
                while !st.done {
                    st = self
                        .stop_writers_cv
                        .wait(st)
                        .unwrap_or_else(|e| e.into_inner());
                }
                return;
            }
            st.started = true;
            true
        };
        if should_run {
            let core = self.core.clone();
            let cap = self.capture.clone();
            let kf = self.keyframes.clone();
            let sched = self.scheduler();

            let t1 = thread::spawn(move || core.stop_pipes());
            let t2 = thread::spawn(move || cap.stop());
            let t3 = thread::spawn(move || kf.stop());
            let t4 = thread::spawn(move || {
                if let Some(s) = sched {
                    s.stop();
                }
            });
            let _ = t1.join();
            let _ = t2.join();
            let _ = t3.join();
            let _ = t4.join();

            let mut st = self.stop_writers.lock().unwrap_or_else(|e| e.into_inner());
            st.done = true;
            self.stop_writers_cv.notify_all();
        }
    }

    pub fn end_session_shutdown(&self) {
        self.cts.cancel();
        {
            let (lock, cv) = &*self.timer_stop;
            let mut stop = lock.lock().unwrap_or_else(|e| e.into_inner());
            *stop = true;
            cv.notify_all();
        }
        let store = self.store();
        if let Some(ref st) = store {
            st.cancel_maintenance();
        }
        self.stop_session_writers();
        self.audio.flush_held();
        if let Some(ref st) = store
            && let Err(e) = st.checkpoint_timeout(Duration::from_millis(1500))
        {
            AgentLog::warn_err("end session: checkpoint", &e.to_string());
        }
    }

    fn on_end_session(&self) {
        AgentLog::info("session ending: stopping pipes and writers, checkpointing the store");
        self.end_session_shutdown();
        AgentLog::flush();
        self.request_quit();
    }

    pub fn shutdown(&self) {
        if let Some(mut w) = self
            .quit_watcher
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            w.stop();
        }
        self.audio.flush_held();
        self.cts.cancel();
        {
            let (lock, cv) = &*self.timer_stop;
            let mut stop = lock.lock().unwrap_or_else(|e| e.into_inner());
            *stop = true;
            cv.notify_all();
        }
        let timers: Vec<JoinHandle<()>> = self
            .timers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain(..)
            .collect();
        for h in timers {
            if h.thread().id() != thread::current().id() {
                let _ = h.join();
            }
        }
        self.stop_session_writers();
        self.audio.stop();
        self.supervisor.dispose();
        if let Some(g) = self.guard.lock().unwrap_or_else(|e| e.into_inner()).take() {
            g.stop();
        }
        *self.probe.lock().unwrap_or_else(|e| e.into_inner()) = None;
        self.thumbs.clear_cache();
        if let Some(w) = self.window.lock().unwrap_or_else(|e| e.into_inner()).take() {
            w.dispose();
        }
        *self
            .settings_guard
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = None;
        *self.purged_guard.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
}

impl AgentControl for AgentHost {
    fn get_status(&self) -> AgentStatus {
        let (reason, until) = self.pause.current();
        let paused = reason.is_some();
        let s = self.settings();
        let stats = self.store_stats();
        let device = if self.supervisor.is_running() {
            self.supervisor
                .hello()
                .map(|h| h.device)
                .unwrap_or_else(|| "none".to_string())
        } else {
            "none".to_string()
        };
        AgentStatus {
            capturing: self.capture_started.load(Ordering::Acquire)
                && !paused
                && s.capture.enabled,
            paused,
            paused_until: until,
            pause_reason: reason,
            locked: !self.is_unlocked(),
            unlocked_until: self.gate.until(),
            audio: self.audio.state(),
            queue: QueueState {
                pending: stats.pending_embeddings,
            },
            device,
            store: stats,
            retention_days: s.retention.days,
            hotkeys_failed: self
                .hotkeys_failed
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone(),
            warnings: self.warnings(),
            user_paused: self.pause.has("user") || self.pause.has("ipc"),
            unlock_needs: None, // filled in by CommandProcessor for UI pipe
        }
    }

    fn pause(&self, duration: Option<Duration>, reason: &str) {
        let r = if reason == "ipc" { "ipc" } else { "user" };
        let until = duration.map(|d| Utc::now() + ChronoDuration::from_std(d).unwrap_or_default());
        self.pause.set(r, until);
        self.audio.flush();
        let desc = match duration {
            None => "until resume".to_string(),
            Some(d) => format!("for {:.0} min", d.as_secs_f64() / 60.0),
        };
        AgentLog::info(&format!("paused ({r}) {desc}"));
        self.raise_status(true);
    }

    fn resume(&self, reason: &str) {
        Self::clear_for_resume(&self.pause, reason);
        let msg = if reason != "ipc" {
            "resumed"
        } else if self.pause.has("user") {
            "resumed (ipc), user pause kept"
        } else {
            "resumed (ipc)"
        };
        AgentLog::info(msg);
        self.raise_status(true);
    }

    fn is_unlocked(&self) -> bool {
        self.gate.is_open(self.settings().security.require_unlock)
    }

    fn unlock(&self, ct: &CancelToken) -> bool {
        let s = self.settings();
        if !s.security.require_unlock {
            self.gate.release_user_lock();
            self.raise_status(true);
            return true;
        }
        let hwnd = self
            .window
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|w| w.handle())
            .unwrap_or(0);
        let cancel_flag = Arc::new(AtomicBool::new(ct.is_cancelled()));
        let flag_cb = cancel_flag.clone();
        let _reg = ct.on_cancel(move || flag_cb.store(true, Ordering::Release));
        let ok = self.core.keys().verify_user(
            hwnd,
            "Unlock BetterRecall to view your history",
            Some(&cancel_flag),
        );
        if ok && !self.core.keys().needs_passphrase() {
            self.gate.open(s.security.unlock_timeout_min);
        }
        self.raise_status(true);
        ok
    }

    fn open_read_gate(&self) {
        self.gate.open(self.settings().security.unlock_timeout_min);
        self.raise_status(true);
    }

    fn lock(&self) {
        self.gate.lock_by_user();
        self.thumbs.clear_cache();
        self.replays.clear_sampler();
        self.raise_status(true);
    }

    fn consent(&self) {
        let patch = serde_json::json!({ "capture": { "consented": true } });
        if let Some(err) = self.core.settings().try_update(&patch) {
            AgentLog::warn_err("consent update failed", &err);
            return;
        }
        *self
            .settings_snapshot
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = self.core.settings().current();
        self.pause.clear("consent");
        self.raise_status(true);
    }

    fn save_replay(&self, ct: &CancelToken) -> Result<ReplayClip, AgentError> {
        self.replays.save_replay(ct)
    }

    fn list_replays(&self) -> Vec<ReplayClip> {
        self.replays.list_replays()
    }

    fn export_replay(&self, id: &str, path: &Path, ct: &CancelToken) -> Result<(), AgentError> {
        self.replays.export_replay(id, path, ct)
    }

    fn delete_replay(&self, id: &str) -> Result<(), AgentError> {
        self.replays.delete_replay(id)?;
        if let Some(st) = self.store()
            && let Err(e) = st.delete_replay_frames(id)
        {
            AgentLog::warn_err("replay frames delete", &e.to_string());
        }
        Ok(())
    }

    fn panic(&self) {
        let sw = Instant::now();
        self.capture.panic();
        self.replays.clear_sampler();
        self.replays.discard_saves_in_flight();
        if let Some(sc) = self.scheduler() {
            sc.clear();
        }
        self.thumbs.clear_cache();
        self.audio.clear();
        self.gate.lock_by_user();
        let minutes = self.settings().privacy.panic_purge_minutes.max(0);
        let mut purged = 0usize;
        if let Some(st) = self.store() {
            match st.purge(&PurgeScope::last(ChronoDuration::minutes(minutes as i64))) {
                Ok(n) => purged = n,
                Err(e) => AgentLog::warn_err("panic purge", &e.to_string()),
            }
        }
        let listeners: Vec<PanicListener> = self
            .panic_listeners
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|(_, cb)| cb.clone())
            .collect();
        for cb in listeners {
            if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| cb())).is_err() {
                AgentLog::warn("panic listener panicked");
            }
        }
        AgentLog::info(&format!(
            "panic flush in {} ms, purged {purged} rows",
            sw.elapsed().as_millis()
        ));
        if let Some(win) = self
            .window
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
        {
            let text = if minutes > 0 {
                format!("Memory flushed, last {minutes} min deleted.")
            } else {
                "Memory flushed.".to_string()
            };
            win.notify("BetterRecall", &text);
        }
        self.raise_status(true);
    }

    fn bench(&self, ct: &CancelToken) -> Result<BenchResult, AgentError> {
        self.worker.bench(ct).map_err(|e| match e {
            WorkerError::Cancelled => AgentError::Cancelled,
            other => AgentError::Other(other.message().to_string()),
        })
    }

    fn bench_full(&self, _ct: &CancelToken) -> Result<BenchReport, AgentError> {
        Err(AgentError::InvalidOperation(
            "Capture is not running.".to_string(),
        ))
    }

    fn on_panicked(&self, cb: Arc<dyn Fn() + Send + Sync>) -> ListenerGuard {
        let id = self.next_listener_id.fetch_add(1, Ordering::Relaxed);
        let list = self.panic_listeners.clone();
        list.lock().unwrap_or_else(|e| e.into_inner()).push((id, cb));
        ListenerGuard::new(move || {
            list.lock()
                .unwrap_or_else(|e| e.into_inner())
                .retain(|(i, _)| *i != id);
        })
    }

    fn on_status_changed(&self, cb: Arc<dyn Fn(&AgentStatus) + Send + Sync>) -> ListenerGuard {
        let id = self.next_listener_id.fetch_add(1, Ordering::Relaxed);
        let list = self.status_listeners.clone();
        list.lock().unwrap_or_else(|e| e.into_inner()).push((id, cb));
        ListenerGuard::new(move || {
            list.lock()
                .unwrap_or_else(|e| e.into_inner())
                .retain(|(i, _)| *i != id);
        })
    }

    fn on_capture_stored(&self, cb: Arc<dyn Fn(&CaptureMeta) + Send + Sync>) -> ListenerGuard {
        let id = self.next_listener_id.fetch_add(1, Ordering::Relaxed);
        let list = self.capture_listeners.clone();
        list.lock().unwrap_or_else(|e| e.into_inner()).push((id, cb));
        ListenerGuard::new(move || {
            list.lock()
                .unwrap_or_else(|e| e.into_inner())
                .retain(|(i, _)| *i != id);
        })
    }
}

fn storage_equal(a: &StorageSettings, b: &StorageSettings) -> bool {
    a.min_free_gb == b.min_free_gb && a.max_gb == b.max_gb && a.overwrite == b.overwrite
}

fn hotkeys_equal(a: &HotkeySettings, b: &HotkeySettings) -> bool {
    a.search == b.search
        && a.pause == b.pause
        && a.replay == b.replay
        && a.panic == b.panic
        && a.timeline == b.timeline
        && a.home == b.home
        && a.lock == b.lock
}
