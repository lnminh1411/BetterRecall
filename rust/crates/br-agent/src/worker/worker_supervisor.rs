// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Worker process supervision: private named pipe, PID verification, hello, backoff, liveness,
//! idle exit, and graceful settings-change restarts (`WorkerSupervisor.cs`).

use std::collections::HashMap;
use std::io::{self, Write};
use std::os::windows::io::AsRawHandle;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicI64, AtomicU32, Ordering};
use std::sync::{Arc, Condvar, LazyLock, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use br_contracts::{PipeNames, WorkerHello, WorkerRequest, WorkerResponse};
use br_core::{CancelSource, CancelToken, PipeStream, win32 as core_win32};
use windows::Win32::Foundation::{HANDLE, HLOCAL, INVALID_HANDLE_VALUE, LocalFree};
use windows::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
use windows::Win32::Storage::FileSystem::{
    FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED, PIPE_ACCESS_DUPLEX,
};
use windows::Win32::System::Pipes::{
    CreateNamedPipeW, GetNamedPipeClientProcessId, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS,
    PIPE_TYPE_BYTE, PIPE_WAIT,
};
use windows::Win32::System::Threading::CREATE_NO_WINDOW;
use windows::core::PCWSTR;

use crate::native::{liveness_now_ms, tick_count64, to_wide_z};
use crate::scheduling::Governance;
use crate::util::AgentLog;
use crate::worker::kill_on_close_job::KillOnCloseJob;
use crate::worker::vc_runtime::VcRuntime;
use crate::worker::worker_connection::{PipeWorkerStream, WorkerConnection, WorkerError};

/// Starts and stops the worker process. Abstracted so tests can run a fake worker in-process.
pub trait WorkerLauncher: Send + Sync {
    /// Starts a worker that connects to `pipe_name`. Returns its process id.
    fn start(&self, pipe_name: &str, home: &Path) -> Result<u32, WorkerError>;
    fn kill(&self, pid: u32);
    fn has_exited(&self, pid: u32) -> bool;
}

static WORKER_JOB: LazyLock<Option<KillOnCloseJob>> = LazyLock::new(|| {
    let j = KillOnCloseJob::try_create();
    if j.is_none() {
        AgentLog::warn("worker job object unavailable: a worker could outlive the agent");
    }
    j
});

/// Snapshot of the last launched worker command line (for tests verifying no settings leak onto argv).
#[derive(Debug, Clone)]
pub struct LastLaunchInfo {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub stdin_redirected: bool,
}

pub type SettingsJsonProvider = Arc<dyn Fn() -> String + Send + Sync>;
pub type ClockFn = Arc<dyn Fn() -> i64 + Send + Sync>;

/// Real process launcher for `worker\BetterRecall.Worker.exe`.
pub struct ProcessWorkerLauncher {
    worker_exe: PathBuf,
    eco_qos: Arc<dyn Fn() -> bool + Send + Sync>,
    settings_json: Option<SettingsJsonProvider>,
    processes: Mutex<HashMap<u32, Child>>,
    last_start_info: Mutex<Option<LastLaunchInfo>>,
}

impl ProcessWorkerLauncher {
    pub const MAX_SETTINGS_BYTES: usize = 64 * 1024;

    pub fn new(
        worker_exe: PathBuf,
        eco_qos: impl Fn() -> bool + Send + Sync + 'static,
        settings_json: Option<SettingsJsonProvider>,
    ) -> Self {
        Self {
            worker_exe,
            eco_qos: Arc::new(eco_qos),
            settings_json,
            processes: Mutex::new(HashMap::new()),
            last_start_info: Mutex::new(None),
        }
    }

    pub fn last_start_info(&self) -> Option<LastLaunchInfo> {
        self.last_start_info
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

impl WorkerLauncher for ProcessWorkerLauncher {
    fn start(&self, pipe_name: &str, home: &Path) -> Result<u32, WorkerError> {
        if !self.worker_exe.is_file() {
            let name = self
                .worker_exe
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("BetterRecall.Worker.exe");
            return Err(WorkerError::Transport(format!("Worker not found: {name}.")));
        }
        if !VcRuntime::present() {
            return Err(WorkerError::Transport(
                VcRuntime::MISSING_MESSAGE.to_string(),
            ));
        }

        let mut args = vec![
            "--pipe".to_string(),
            pipe_name.to_string(),
            "--home".to_string(),
            home.to_string_lossy().into_owned(),
        ];

        let settings_bytes = self
            .settings_json
            .as_ref()
            .map(|f| f().into_bytes());

        if let Some(ref bytes) = settings_bytes {
            if bytes.len() > Self::MAX_SETTINGS_BYTES {
                return Err(WorkerError::Transport(
                    "Worker settings too large.".to_string(),
                ));
            }
            args.push("--settings-stdin".to_string());
        }

        // Reap exited old children.
        {
            let mut procs = self.processes.lock().unwrap_or_else(|e| e.into_inner());
            procs.retain(|_, child| !matches!(child.try_wait(), Ok(Some(_))));
        }

        *self
            .last_start_info
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(LastLaunchInfo {
            program: self.worker_exe.clone(),
            args: args.clone(),
            stdin_redirected: settings_bytes.is_some(),
        });

        let mut cmd = Command::new(&self.worker_exe);
        cmd.args(&args).creation_flags(CREATE_NO_WINDOW.0);
        if let Some(dir) = self.worker_exe.parent() {
            cmd.current_dir(dir);
        }
        if settings_bytes.is_some() {
            cmd.stdin(Stdio::piped());
        } else {
            cmd.stdin(Stdio::null());
        }
        cmd.stdout(Stdio::null()).stderr(Stdio::null());

        let mut child = cmd
            .spawn()
            .map_err(|_| WorkerError::Transport("Worker failed to start.".to_string()))?;
        let pid = child.id();
        let raw_handle = HANDLE(child.as_raw_handle());

        if let Some(ref job) = *WORKER_JOB
            && !unsafe { job.assign(raw_handle) }
        {
            AgentLog::warn("worker not in the job object: it could outlive the agent");
        }
        if (self.eco_qos)() {
            unsafe {
                Governance::apply_eco_qos(raw_handle, true);
            }
        }

        if let Some(bytes) = settings_bytes {
            let write_res = if let Some(mut stdin) = child.stdin.take() {
                stdin.write_all(&bytes).and_then(|()| stdin.flush())
            } else {
                Err(io::Error::new(io::ErrorKind::BrokenPipe, "no stdin"))
            };
            if write_res.is_err() {
                let _ = child.kill();
                let _ = child.wait();
                return Err(WorkerError::Transport(
                    "Worker did not take its settings.".to_string(),
                ));
            }
        }

        self.processes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(pid, child);
        Ok(pid)
    }

    fn kill(&self, pid: u32) {
        let mut procs = self.processes.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(child) = procs.get_mut(&pid)
            && !matches!(child.try_wait(), Ok(Some(_)))
        {
            let _ = child.kill();
        }
    }

    fn has_exited(&self, pid: u32) -> bool {
        let mut procs = self.processes.lock().unwrap_or_else(|e| e.into_inner());
        match procs.get_mut(&pid) {
            None => true,
            Some(child) => matches!(child.try_wait(), Ok(Some(_))),
        }
    }
}

pub type HelloCallback = Arc<dyn Fn(&WorkerHello) + Send + Sync>;

struct StartState {
    conn: Option<Arc<WorkerConnection>>,
    in_flight: Option<Arc<SharedStart>>,
    pid: u32,
}

struct SharedStart {
    result: Mutex<Option<Result<Arc<WorkerConnection>, WorkerError>>>,
    cv: Condvar,
}

impl SharedStart {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            result: Mutex::new(None),
            cv: Condvar::new(),
        })
    }

    fn complete(&self, res: Result<Arc<WorkerConnection>, WorkerError>) {
        let mut g = self.result.lock().unwrap_or_else(|e| e.into_inner());
        *g = Some(res);
        self.cv.notify_all();
    }

    fn wait(self: &Arc<Self>, ct: &CancelToken) -> Result<Arc<WorkerConnection>, WorkerError> {
        let me = self.clone();
        let _reg = ct.on_cancel(move || {
            let _g = me.result.lock().unwrap_or_else(|e| e.into_inner());
            me.cv.notify_all();
        });
        let mut g = self.result.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if let Some(ref res) = *g {
                return res.clone();
            }
            if ct.is_cancelled() {
                return Err(WorkerError::Cancelled);
            }
            g = self.cv.wait(g).unwrap_or_else(|e| e.into_inner());
        }
    }
}

/// Configuration knobs for [`WorkerSupervisor`] (matching C# init properties).
pub struct SupervisorOptions {
    pub connect_timeout: Duration,
    pub hello_timeout: Duration,
    pub verify_client_pid: bool,
    pub unresponsive_after: Duration,
    pub liveness_clock: Option<ClockFn>,
    pub check_clock: Option<ClockFn>,
    pub start_timer: bool,
}

impl Default for SupervisorOptions {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(30),
            hello_timeout: Duration::from_secs(600),
            verify_client_pid: true,
            unresponsive_after: Duration::from_secs(90),
            liveness_clock: None,
            check_clock: None,
            start_timer: true,
        }
    }
}

struct SupervisorInner {
    launcher: Arc<dyn WorkerLauncher>,
    home: PathBuf,
    idle_exit_s: Arc<dyn Fn() -> i32 + Send + Sync>,
    opts: SupervisorOptions,
    start_gate: Mutex<StartState>,
    lifetime: CancelSource,
    failures: AtomicI32,
    next_attempt_ticks: AtomicI64,
    disposed: AtomicBool,
    restart_for: Mutex<Option<String>>,
    hello: Mutex<Option<WorkerHello>>,
    hello_listeners: Mutex<Vec<(u64, HelloCallback)>>,
    next_hello_id: AtomicI64,
    last_check_ms: AtomicI64,
    grace_until_ms: AtomicI64,
    recycled: AtomicI32,
    last_pid: AtomicU32,
    timer_stop: Mutex<bool>,
    timer_cv: Condvar,
}

/// Owns the worker process: private pipe (DACL current user only, NETWORK denied), launch with
/// `--pipe`/`--home`, client PID check, hello, restart with backoff after crash, shutdown when idle.
pub struct WorkerSupervisor {
    inner: Arc<SupervisorInner>,
    timer_thread: Mutex<Option<JoinHandle<()>>>,
}

impl WorkerSupervisor {
    pub const FROZEN_GAP_MS: i64 = 30_000;
    pub const FROZEN_GRACE_MS: i64 = 45_000;

    pub fn new(
        launcher: Arc<dyn WorkerLauncher>,
        home: PathBuf,
        idle_exit_s: impl Fn() -> i32 + Send + Sync + 'static,
    ) -> Arc<Self> {
        Self::with_options(launcher, home, idle_exit_s, SupervisorOptions::default())
    }

    pub fn with_options(
        launcher: Arc<dyn WorkerLauncher>,
        home: PathBuf,
        idle_exit_s: impl Fn() -> i32 + Send + Sync + 'static,
        opts: SupervisorOptions,
    ) -> Arc<Self> {
        let start_timer = opts.start_timer;
        let inner = Arc::new(SupervisorInner {
            launcher,
            home,
            idle_exit_s: Arc::new(idle_exit_s),
            opts,
            start_gate: Mutex::new(StartState {
                conn: None,
                in_flight: None,
                pid: 0,
            }),
            lifetime: CancelSource::new(),
            failures: AtomicI32::new(0),
            next_attempt_ticks: AtomicI64::new(0),
            disposed: AtomicBool::new(false),
            restart_for: Mutex::new(None),
            hello: Mutex::new(None),
            hello_listeners: Mutex::new(Vec::new()),
            next_hello_id: AtomicI64::new(1),
            last_check_ms: AtomicI64::new(i64::MIN),
            grace_until_ms: AtomicI64::new(i64::MIN),
            recycled: AtomicI32::new(0),
            last_pid: AtomicU32::new(0),
            timer_stop: Mutex::new(false),
            timer_cv: Condvar::new(),
        });

        let sup = Arc::new(Self {
            inner: inner.clone(),
            timer_thread: Mutex::new(None),
        });

        if start_timer {
            let weak = Arc::downgrade(&sup);
            let h = thread::Builder::new()
                .name("BetterRecall.WorkerSupervisor".to_string())
                .spawn(move || {
                    loop {
                        let Some(s) = weak.upgrade() else { return };
                        {
                            let stop = s
                                .inner
                                .timer_stop
                                .lock()
                                .unwrap_or_else(|e| e.into_inner());
                            if *stop {
                                return;
                            }
                            let (stop, _) = s
                                .inner
                                .timer_cv
                                .wait_timeout(stop, Duration::from_secs(5))
                                .unwrap_or_else(|e| e.into_inner());
                            if *stop {
                                return;
                            }
                        }
                        s.check_idle();
                    }
                })
                .ok();
            *sup.timer_thread.lock().unwrap_or_else(|e| e.into_inner()) = h;
        }

        sup
    }

    /// SDDL for the private worker pipe: owner current user, protected DACL, Deny NETWORK, Allow current user.
    pub fn pipe_sddl(sid: &str) -> String {
        format!("O:{sid}D:P(D;;GA;;;NU)(A;;GA;;;{sid})")
    }

    pub fn on_hello_received(&self, cb: HelloCallback) -> u64 {
        let id = self.inner.next_hello_id.fetch_add(1, Ordering::Relaxed) as u64;
        self.inner
            .hello_listeners
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push((id, cb));
        id
    }

    pub fn remove_hello_listener(&self, id: u64) {
        self.inner
            .hello_listeners
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|(i, _)| *i != id);
    }

    pub fn hello(&self) -> Option<WorkerHello> {
        self.inner
            .hello
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn is_running(&self) -> bool {
        self.inner
            .start_gate
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .conn
            .as_ref()
            .is_some_and(|c| !c.is_closed())
    }

    pub fn pid(&self) -> u32 {
        self.inner.last_pid.load(Ordering::Acquire)
    }

    pub fn failures(&self) -> i32 {
        self.inner.failures.load(Ordering::Acquire)
    }

    pub fn recycled(&self) -> i32 {
        self.inner.recycled.load(Ordering::Acquire)
    }

    pub fn restart_pending(&self) -> bool {
        self.inner
            .restart_for
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some()
    }

    pub fn mark_healthy(&self) {
        self.inner.failures.store(0, Ordering::Release);
    }

    /// Starts the worker when not running. Shared start runs under the supervisor's lifetime
    /// (never cancelled by a caller's `ct`; `ct` only ends this caller's wait).
    pub fn ensure(self: &Arc<Self>, ct: &CancelToken) -> Result<Arc<WorkerConnection>, WorkerError> {
        let shared = {
            let mut st = self
                .inner
                .start_gate
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if let Some(ref c) = st.conn
                && !c.is_closed()
            {
                return Ok(c.clone());
            }
            if self.inner.disposed.load(Ordering::Acquire) {
                return Err(WorkerError::Transport(
                    "WorkerSupervisor is disposed.".to_string(),
                ));
            }
            if let Some(ref s) = st.in_flight {
                s.clone()
            } else {
                let wait = self
                    .inner
                    .next_attempt_ticks
                    .load(Ordering::Acquire)
                    .saturating_sub(tick_count64());
                if wait > 0 {
                    return Err(WorkerError::Transport(format!(
                        "Worker restart backoff ({wait} ms)."
                    )));
                }
                let s = SharedStart::new();
                st.in_flight = Some(s.clone());
                let sup = self.clone();
                let s_bg = s.clone();
                let _ = thread::Builder::new()
                    .name("BetterRecall.WorkerStart".to_string())
                    .spawn(move || {
                        let res = sup.start_shared();
                        {
                            let mut g = sup
                                .inner
                                .start_gate
                                .lock()
                                .unwrap_or_else(|e| e.into_inner());
                            if let Ok(ref conn) = res
                                && !sup.inner.disposed.load(Ordering::Acquire)
                            {
                                g.conn = Some(conn.clone());
                            }
                            g.in_flight = None;
                        }
                        s_bg.complete(res);
                    });
                s
            }
        };
        shared.wait(ct)
    }

    fn start_shared(self: &Arc<Self>) -> Result<Arc<WorkerConnection>, WorkerError> {
        match self.start_once(&self.inner.lifetime.token()) {
            Ok(c) => {
                if self.inner.disposed.load(Ordering::Acquire) {
                    c.dispose();
                    return Err(WorkerError::Transport(
                        "WorkerSupervisor is disposed.".to_string(),
                    ));
                }
                Ok(c)
            }
            Err(e) => {
                if !self.inner.disposed.load(Ordering::Acquire) {
                    self.note_failure();
                    AgentLog::warn_err("worker start failed", e.message());
                }
                Err(match e {
                    WorkerError::Cancelled => {
                        WorkerError::Transport("Worker start timed out.".to_string())
                    }
                    other => other,
                })
            }
        }
    }

    fn note_failure(&self) {
        let f = self.inner.failures.fetch_add(1, Ordering::AcqRel) + 1;
        let shift = (f - 1).clamp(0, 6) as u32;
        let delay = 60_000i64.min(1000i64 << shift);
        self.inner
            .next_attempt_ticks
            .store(tick_count64() + delay, Ordering::Release);
    }

    fn start_once(
        self: &Arc<Self>,
        ct: &CancelToken,
    ) -> Result<Arc<WorkerConnection>, WorkerError> {
        let name = PipeNames::new_worker();
        let server = create_worker_pipe(&name)
            .map_err(|e| WorkerError::Transport(format!("Worker pipe create failed: {e}")))?;
        let cleanup = |server: &PipeStream, pid: u32, launcher: &Arc<dyn WorkerLauncher>| {
            server.close();
            if pid != 0 {
                launcher.kill(pid);
            }
        };

        let sw = Instant::now();
        let pid = match self.inner.launcher.start(&name, &self.inner.home) {
            Ok(p) => p,
            Err(e) => {
                server.close();
                return Err(e);
            }
        };
        self.inner.last_pid.store(pid, Ordering::Release);
        {
            let mut st = self
                .inner
                .start_gate
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            st.pid = pid;
        }

        // Wait for connection with connect_timeout linked to ct.
        let conn_res = wait_with_timeout(self.inner.opts.connect_timeout, ct, |t| {
            server.wait_for_connection(t)
        });
        if conn_res.is_err() {
            cleanup(&server, pid, &self.inner.launcher);
            return Err(WorkerError::Transport("Worker start timed out.".to_string()));
        }

        if self.inner.opts.verify_client_pid && client_pid(&server) != pid {
            cleanup(&server, pid, &self.inner.launcher);
            return Err(WorkerError::Transport(
                "Worker pipe client is not the started process.".to_string(),
            ));
        }

        let clock = self
            .inner
            .opts
            .liveness_clock
            .clone()
            .unwrap_or_else(|| Arc::new(liveness_now_ms));
        let conn = WorkerConnection::new(Arc::new(PipeWorkerStream::new(server.clone())), Some(clock));
        let sup_weak = Arc::downgrade(self);
        let conn_for_cb = conn.clone();
        let conn_pid = pid;
        conn.set_closed_handler(Arc::new(move |err, pending| {
            if let Some(sup) = sup_weak.upgrade() {
                sup.on_closed(&conn_for_cb, conn_pid, err, pending);
            }
        }));

        let hello_res = wait_with_timeout(self.inner.opts.hello_timeout, ct, |t| {
            conn.send(
                WorkerRequest {
                    id: 0,
                    op: "hello".to_string(),
                    ..Default::default()
                },
                &[],
                t,
                false,
            )
        });
        let reply = match hello_res {
            Ok(r) => r,
            Err(e) => {
                conn.dispose();
                cleanup(&server, pid, &self.inner.launcher);
                return Err(e);
            }
        };
        if !reply.header.ok {
            let msg = reply
                .header
                .error
                .unwrap_or_else(|| "Worker hello failed.".to_string());
            conn.dispose();
            cleanup(&server, pid, &self.inner.launcher);
            return Err(WorkerError::Transport(msg));
        }
        let Some(hello) = reply.header.hello else {
            conn.dispose();
            cleanup(&server, pid, &self.inner.launcher);
            return Err(WorkerError::Transport("Worker hello failed.".to_string()));
        };

        *self.inner.hello.lock().unwrap_or_else(|e| e.into_inner()) = Some(hello.clone());
        AgentLog::info(&format!(
            "worker up pid={pid} device={} ep={} dim={} nsfw={} in {} ms",
            hello.device,
            hello.ep,
            hello.dim,
            hello.nsfw_device.as_deref().unwrap_or("not loaded"),
            sw.elapsed().as_millis()
        ));
        self.raise_hello(&hello);
        Ok(conn)
    }

    fn raise_hello(&self, hello: &WorkerHello) {
        let listeners: Vec<HelloCallback> = self
            .inner
            .hello_listeners
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|(_, cb)| cb.clone())
            .collect();
        for cb in listeners {
            if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| cb(hello))).is_err() {
                AgentLog::warn("hello handler panicked");
            }
        }
    }

    fn on_closed(
        &self,
        conn: &Arc<WorkerConnection>,
        pid: u32,
        err: Option<String>,
        pending_at_close: usize,
    ) {
        let crashed = err.is_some() || pending_at_close > 0;
        if let Some(ref e) = err {
            AgentLog::warn(&format!("worker pipe closed with error {e}"));
        } else {
            AgentLog::info("worker pipe closed");
        }
        if conn.recycled() {
            // already counted
        } else if crashed
            && !self.inner.disposed.load(Ordering::Acquire)
            && !conn.shutting_down()
        {
            self.note_failure();
        } else {
            self.inner.failures.store(0, Ordering::Release);
        }
        {
            let mut st = self
                .inner
                .start_gate
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if st.conn.as_ref().is_some_and(|c| Arc::ptr_eq(c, conn)) {
                st.conn = None;
            }
        }
        if crashed && !conn.recycled() && !conn.shutting_down() && pid != 0 {
            self.inner.launcher.kill(pid);
        }
        let conn_bg = conn.clone();
        let _ = thread::Builder::new()
            .name("BetterRecall.WorkerDispose".to_string())
            .spawn(move || conn_bg.dispose());
    }

    /// Updates `Hello` and notifies listeners when a reply reports a changed model/revision/device.
    pub fn note_embedder(&self, r: &WorkerResponse) {
        let (Some(model), Some(revision)) = (r.model.as_deref(), r.revision.as_deref()) else {
            return;
        };
        if model.is_empty() || revision.is_empty() {
            return;
        }
        let next = {
            let mut g = self.inner.hello.lock().unwrap_or_else(|e| e.into_inner());
            let Some(ref h) = *g else { return };
            let device = r.device.as_deref().unwrap_or(&h.device);
            if h.model == model && h.revision == revision && h.device == device {
                return;
            }
            let mut updated = h.clone();
            updated.model = model.to_string();
            updated.revision = revision.to_string();
            updated.device = device.to_string();
            if let Some(dim) = r.dim {
                updated.dim = dim;
            }
            let line = format!(
                "worker embedder now {} rev {} on {} (was {} rev {} on {})",
                updated.model, updated.revision, updated.device, h.model, h.revision, h.device
            );
            if !h.revision.is_empty() && (h.model != updated.model || h.revision != updated.revision)
            {
                AgentLog::warn(&format!("{line}; stored rows will be embedded again"));
            } else {
                AgentLog::info(&line);
            }
            *g = Some(updated.clone());
            updated
        };
        self.raise_hello(&next);
    }

    pub fn request_restart(self: &Arc<Self>, why: &str) {
        if self.inner.disposed.load(Ordering::Acquire) {
            return;
        }
        *self
            .inner
            .restart_for
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(why.to_string());
        self.check_idle();
    }

    pub fn check_now(self: &Arc<Self>) {
        self.check_idle();
    }

    pub fn is_unresponsive(serving_op: &str, serving_ms: i64, silence_ms: i64) -> bool {
        serving_op != "bench" && serving_op != "hello" && serving_ms >= silence_ms
    }

    fn check_idle(self: &Arc<Self>) {
        let now_ms = match self.inner.opts.check_clock {
            Some(ref f) => f(),
            None => tick_count64(),
        };
        let last = self.inner.last_check_ms.swap(now_ms, Ordering::AcqRel);
        if last != i64::MIN && now_ms.saturating_sub(last) > Self::FROZEN_GAP_MS {
            self.inner
                .grace_until_ms
                .store(now_ms + Self::FROZEN_GRACE_MS, Ordering::Release);
            AgentLog::info(&format!(
                "liveness: agent did not run for {} s (sleep, standby or freeze), grace {} s",
                (now_ms - last) / 1000,
                Self::FROZEN_GRACE_MS / 1000
            ));
        }
        let grace = now_ms < self.inner.grace_until_ms.load(Ordering::Acquire);

        let c = {
            let st = self
                .inner
                .start_gate
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if st.conn.as_ref().is_none_or(|c| c.is_closed()) {
                if st.in_flight.is_none() {
                    *self
                        .inner
                        .restart_for
                        .lock()
                        .unwrap_or_else(|e| e.into_inner()) = None;
                }
                return;
            }
            st.conn.clone().unwrap()
        };

        if !grace
            && let Some((op, ms)) = c.oldest()
            && Self::is_unresponsive(
                &op,
                ms,
                self.inner.opts.unresponsive_after.as_millis() as i64,
            )
        {
            AgentLog::warn(&format!(
                "worker not answering ({op} for {} s, {} waiting), restarting it",
                ms / 1000,
                c.pending()
            ));
            let sup = self.clone();
            let _ = thread::Builder::new()
                .name("BetterRecall.WorkerRecycle".to_string())
                .spawn(move || sup.recycle(&c));
            return;
        }

        if c.pending() > 0 {
            return;
        }
        let restart_why = self
            .inner
            .restart_for
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        if let Some(why) = restart_why {
            AgentLog::info(&format!("worker restart: {why} changed"));
            let sup = self.clone();
            let _ = thread::Builder::new()
                .name("BetterRecall.WorkerRestart".to_string())
                .spawn(move || sup.shutdown());
            return;
        }
        let idle_limit_ms = ((self.inner.idle_exit_s)() as i64 + 15) * 1000;
        if c.idle_ms() > idle_limit_ms {
            AgentLog::info("worker idle, sending shutdown");
            let sup = self.clone();
            let _ = thread::Builder::new()
                .name("BetterRecall.WorkerIdleStop".to_string())
                .spawn(move || sup.shutdown());
        }
    }

    fn recycle(&self, c: &Arc<WorkerConnection>) {
        let pid = {
            let mut st = self
                .inner
                .start_gate
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if !st.conn.as_ref().is_some_and(|cur| Arc::ptr_eq(cur, c)) || c.recycled() {
                return;
            }
            self.note_failure();
            let p = st.pid;
            c.set_recycled(true);
            st.conn = None;
            p
        };
        self.inner.recycled.fetch_add(1, Ordering::AcqRel);
        if pid != 0 {
            self.inner.launcher.kill(pid);
        }
        c.dispose();
    }

    /// Stops this worker without counting a failure (no backoff): e.g. cancelled bench.
    pub fn abort(&self, c: &Arc<WorkerConnection>, why: &str) {
        let pid = {
            let mut st = self
                .inner
                .start_gate
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if !st.conn.as_ref().is_some_and(|cur| Arc::ptr_eq(cur, c)) || c.recycled() {
                return;
            }
            let p = st.pid;
            c.set_aborted(true);
            c.set_recycled(true);
            st.conn = None;
            p
        };
        AgentLog::info(&format!("worker stopped: {why}"));
        if pid != 0 {
            self.inner.launcher.kill(pid);
        }
        c.dispose();
    }

    pub fn shutdown(&self) {
        let (c, pid) = {
            let mut st = self
                .inner
                .start_gate
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let Some(conn) = st.conn.take() else {
                return;
            };
            (conn, st.pid)
        };
        c.set_shutting_down(true);
        let _ = wait_with_timeout(Duration::from_secs(2), &CancelToken::none(), |t| {
            c.send(
                WorkerRequest {
                    id: 0,
                    op: "shutdown".to_string(),
                    ..Default::default()
                },
                &[],
                t,
                false,
            )
        });
        c.dispose();
        if pid != 0 {
            for _ in 0..20 {
                if self.inner.launcher.has_exited(pid) {
                    break;
                }
                thread::sleep(Duration::from_millis(100));
            }
            if !self.inner.launcher.has_exited(pid) {
                self.inner.launcher.kill(pid);
            }
        }
    }

    pub fn dispose(&self) {
        self.inner.disposed.store(true, Ordering::Release);
        {
            let mut stop = self
                .inner
                .timer_stop
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            *stop = true;
            self.inner.timer_cv.notify_all();
        }
        if let Some(h) = self
            .timer_thread
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
            && h.thread().id() != thread::current().id()
        {
            let _ = h.join();
        }
        self.inner.lifetime.cancel();
        let in_flight = self
            .inner
            .start_gate
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .in_flight
            .clone();
        if let Some(s) = in_flight {
            let _ = s.wait(&CancelToken::none());
        }
        self.shutdown();
    }
}

impl Drop for WorkerSupervisor {
    fn drop(&mut self) {
        self.dispose();
    }
}

/// Creates a private single-instance overlapped named pipe for the worker with user-only DACL
/// and explicit NETWORK deny.
pub fn create_worker_pipe(name: &str) -> io::Result<PipeStream> {
    let sid = PipeNames::user_sid()?;
    let sddl = WorkerSupervisor::pipe_sddl(&sid);
    let sddl_w = to_wide_z(&sddl);
    let mut sd = PSECURITY_DESCRIPTOR::default();
    unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            PCWSTR(sddl_w.as_ptr()),
            SDDL_REVISION_1,
            &mut sd,
            None,
        )
    }
    .map_err(core_win32::win32_to_io)?;

    let sa = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: sd.0,
        bInheritHandle: false.into(),
    };

    let full_name = format!(r"\\.\pipe\{}", PipeNames::base(name));
    let full_w = to_wide_z(&full_name);
    let open_mode = PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED | FILE_FLAG_FIRST_PIPE_INSTANCE;
    let pipe_mode = PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS;
    let handle = unsafe {
        CreateNamedPipeW(
            PCWSTR(full_w.as_ptr()),
            open_mode,
            pipe_mode,
            1,
            0,
            0,
            0,
            Some(&sa),
        )
    };
    unsafe {
        let _ = LocalFree(Some(HLOCAL(sd.0)));
    }
    if handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    unsafe { PipeStream::from_raw_handle(handle) }
}

fn client_pid(server: &PipeStream) -> u32 {
    let mut pid = 0u32;
    if unsafe { GetNamedPipeClientProcessId(server.raw_handle(), &mut pid) }.is_ok() {
        pid
    } else {
        0
    }
}

/// Runs `op` with a child `CancelToken` cancelled when either `parent` cancels or `timeout` elapses.
fn wait_with_timeout<T, E>(
    timeout: Duration,
    parent: &CancelToken,
    op: impl FnOnce(&CancelToken) -> Result<T, E>,
) -> Result<T, E> {
    let child = Arc::new(CancelSource::new());
    let child_for_parent = child.clone();
    let _reg = parent.on_cancel(move || child_for_parent.cancel());

    let done = Arc::new((Mutex::new(false), Condvar::new()));
    let done_bg = done.clone();
    let child_bg = child.clone();
    let timer = thread::Builder::new()
        .name("BetterRecall.Timeout".to_string())
        .spawn(move || {
            let (lock, cv) = &*done_bg;
            let g = lock.lock().unwrap_or_else(|e| e.into_inner());
            if *g {
                return;
            }
            let (g2, _) = cv
                .wait_timeout(g, timeout)
                .unwrap_or_else(|e| e.into_inner());
            if !*g2 {
                child_bg.cancel();
            }
        })
        .ok();

    let res = op(&child.token());
    {
        let (lock, cv) = &*done;
        let mut g = lock.lock().unwrap_or_else(|e| e.into_inner());
        *g = true;
        cv.notify_all();
    }
    if let Some(h) = timer {
        let _ = h.join();
    }
    res
}
