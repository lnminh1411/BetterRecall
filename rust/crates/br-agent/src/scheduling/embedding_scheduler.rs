// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Drains `take_pending_embeddings` into the worker under governance (`EmbeddingScheduler.cs`).

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use br_contracts::{CaptureKind, EmbeddingInfo, PendingJob, Settings, WorkerHello};
use br_core::{CancelSource, CancelToken, EncryptedStore};
use zeroize::Zeroize;

use crate::native::tick_count64;
use crate::scheduling::governance::{Governance, GovernanceInputs};
use crate::seams::{ImageDecoder, UnavailableImageDecoder, commonly_decodable};
use crate::util::AgentLog;
use crate::worker::{WorkerClient, WorkerError};

/// Minimal store surface used by [`EmbeddingScheduler`] (implemented for `EncryptedStore` and test fakes).
pub trait EmbeddingStore: Send + Sync {
    fn store_identity(&self) -> usize;
    fn ensure_embedding_space(&self, info: &EmbeddingInfo) -> Result<(), String>;
    fn take_pending_embeddings(&self, max: usize) -> Result<Vec<PendingJob>, String>;
    fn set_embedding(&self, id: &str, vector: &[f32], space: &EmbeddingInfo) -> Result<(), String>;
    fn fail_embedding(&self, id: &str, reason: &str) -> Result<(), String>;
    fn drop_capture(&self, id: &str, reason: &str) -> Result<(), String>;
}

impl EmbeddingStore for EncryptedStore {
    fn store_identity(&self) -> usize {
        (self as *const EncryptedStore) as usize
    }

    fn ensure_embedding_space(&self, info: &EmbeddingInfo) -> Result<(), String> {
        self.ensure_embedding_space(info).map_err(|e| e.to_string())
    }

    fn take_pending_embeddings(&self, max: usize) -> Result<Vec<PendingJob>, String> {
        self.take_pending_embeddings(max).map_err(|e| e.to_string())
    }

    fn set_embedding(&self, id: &str, vector: &[f32], space: &EmbeddingInfo) -> Result<(), String> {
        self.set_embedding(id, vector, space)
            .map_err(|e| e.to_string())
    }

    fn fail_embedding(&self, id: &str, reason: &str) -> Result<(), String> {
        self.fail_embedding(id, reason).map_err(|e| e.to_string())
    }

    fn drop_capture(&self, id: &str, reason: &str) -> Result<(), String> {
        self.drop_capture(id, reason).map_err(|e| e.to_string())
    }
}

struct QueueState {
    queue: VecDeque<PendingJob>,
    in_flight: Option<String>,
    in_flight_dropped: bool,
}

struct WakeState {
    signaled: bool,
    hello_seq: i64,
    step_hello_seq: i64,
    backing_off: bool,
}

struct SpaceState {
    space: Option<EmbeddingInfo>,
    space_store_id: Option<usize>,
}

type StoreProvider = Arc<dyn Fn() -> Option<Arc<dyn EmbeddingStore>> + Send + Sync>;
type SettingsProvider = Arc<dyn Fn() -> Settings + Send + Sync>;
type ProbeProvider = Arc<dyn Fn() -> GovernanceInputs + Send + Sync>;
type PausedProvider = Arc<dyn Fn() -> bool + Send + Sync>;
type IndexDimProvider = Arc<dyn Fn() -> i32 + Send + Sync>;

struct SchedulerInner {
    store: StoreProvider,
    settings: SettingsProvider,
    worker: Arc<WorkerClient>,
    probe: ProbeProvider,
    paused: PausedProvider,
    index_dim: Option<IndexDimProvider>,
    decoder: Arc<dyn ImageDecoder>,
    qstate: Mutex<QueueState>,
    wake: Mutex<WakeState>,
    wake_cv: Condvar,
    space: Mutex<SpaceState>,
    attempts: Mutex<HashMap<String, i32>>,
    dim_restart_ticks: AtomicI64,
    space_refused_logged: AtomicBool,
    last_decision: Mutex<String>,
    processed: AtomicUsize,
    dropped: AtomicUsize,
    cts: CancelSource,
    hello_listener_id: AtomicI64,
}

pub struct EmbeddingScheduler {
    inner: Arc<SchedulerInner>,
    loop_thread: Mutex<Option<JoinHandle<()>>>,
}

impl EmbeddingScheduler {
    pub const MAX_TRANSIENT_ATTEMPTS: i32 = 5;
    pub const MAX_CRASH_ATTEMPTS: i32 = 3;

    pub fn new(
        store: StoreProvider,
        settings: SettingsProvider,
        worker: Arc<WorkerClient>,
        probe: ProbeProvider,
        paused: PausedProvider,
        index_dim: Option<IndexDimProvider>,
    ) -> Arc<Self> {
        Self::with_decoder(
            store,
            settings,
            worker,
            probe,
            paused,
            index_dim,
            Arc::new(UnavailableImageDecoder),
        )
    }

    pub fn with_decoder(
        store: StoreProvider,
        settings: SettingsProvider,
        worker: Arc<WorkerClient>,
        probe: ProbeProvider,
        paused: PausedProvider,
        index_dim: Option<IndexDimProvider>,
        decoder: Arc<dyn ImageDecoder>,
    ) -> Arc<Self> {
        let inner = Arc::new(SchedulerInner {
            store,
            settings,
            worker: worker.clone(),
            probe,
            paused,
            index_dim,
            decoder,
            qstate: Mutex::new(QueueState {
                queue: VecDeque::new(),
                in_flight: None,
                in_flight_dropped: false,
            }),
            wake: Mutex::new(WakeState {
                signaled: false,
                hello_seq: 0,
                step_hello_seq: 0,
                backing_off: false,
            }),
            wake_cv: Condvar::new(),
            space: Mutex::new(SpaceState {
                space: None,
                space_store_id: None,
            }),
            attempts: Mutex::new(HashMap::new()),
            dim_restart_ticks: AtomicI64::new(i64::MIN / 2),
            space_refused_logged: AtomicBool::new(false),
            last_decision: Mutex::new("idle".to_string()),
            processed: AtomicUsize::new(0),
            dropped: AtomicUsize::new(0),
            cts: CancelSource::new(),
            hello_listener_id: AtomicI64::new(0),
        });

        let sched = Arc::new(Self {
            inner: inner.clone(),
            loop_thread: Mutex::new(None),
        });

        let weak = Arc::downgrade(&sched);
        let lid = worker.supervisor().on_hello_received(Arc::new(move |h| {
            if let Some(s) = weak.upgrade() {
                s.on_hello(h);
            }
        }));
        inner.hello_listener_id.store(lid as i64, Ordering::Release);
        if let Some(h) = worker.supervisor().hello() {
            sched.on_hello(&h);
        }
        sched
    }

    pub fn last_decision(&self) -> String {
        self.inner
            .last_decision
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn processed(&self) -> usize {
        self.inner.processed.load(Ordering::Acquire)
    }

    pub fn dropped(&self) -> usize {
        self.inner.dropped.load(Ordering::Acquire)
    }

    pub fn queued(&self) -> usize {
        self.inner
            .qstate
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .queue
            .len()
    }

    pub fn attempts(&self, id: &str) -> i32 {
        *self
            .inner
            .attempts
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(id)
            .unwrap_or(&0)
    }

    pub fn is_transient(err: &WorkerError) -> bool {
        matches!(err, WorkerError::Reply(m) if m.starts_with("Device failed:") || m.starts_with("Internal error."))
    }

    pub fn is_crash(err: &WorkerError) -> bool {
        matches!(err, WorkerError::Transport(m) if m.starts_with("Worker pipe closed"))
    }

    pub fn start(self: &Arc<Self>) {
        let mut slot = self.loop_thread.lock().unwrap_or_else(|e| e.into_inner());
        if slot.is_some() {
            return;
        }
        let me = self.clone();
        *slot = thread::Builder::new()
            .name("BetterRecall.EmbeddingScheduler".to_string())
            .spawn(move || me.run_loop())
            .ok();
    }

    pub fn signal(&self) {
        let mut w = self.inner.wake.lock().unwrap_or_else(|e| e.into_inner());
        if !w.backing_off {
            w.signaled = true;
            self.inner.wake_cv.notify_one();
        }
    }

    pub fn clear(&self) {
        {
            let mut q = self.inner.qstate.lock().unwrap_or_else(|e| e.into_inner());
            for j in q.queue.iter_mut() {
                if let Some(ref mut img) = j.image {
                    img.zeroize();
                }
            }
            q.queue.clear();
            if q.in_flight.is_some() {
                q.in_flight_dropped = true;
            }
        }
        self.inner
            .attempts
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
    }

    pub fn remove(&self, ids: &HashSet<String>) -> usize {
        if ids.is_empty() {
            return 0;
        }
        let mut q = self.inner.qstate.lock().unwrap_or_else(|e| e.into_inner());
        let before = q.queue.len();
        let mut keep = VecDeque::with_capacity(before);
        while let Some(mut j) = q.queue.pop_front() {
            if ids.contains(&j.id) {
                if let Some(ref mut img) = j.image {
                    img.zeroize();
                }
            } else {
                keep.push_back(j);
            }
        }
        q.queue = keep;
        if let Some(ref f) = q.in_flight
            && ids.contains(f)
        {
            q.in_flight_dropped = true;
        }
        before - q.queue.len()
    }

    pub fn enqueue_for_test(&self, jobs: impl IntoIterator<Item = PendingJob>) {
        let mut q = self.inner.qstate.lock().unwrap_or_else(|e| e.into_inner());
        q.queue.extend(jobs);
    }

    fn set_decision(&self, d: &str) {
        *self
            .inner
            .last_decision
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = d.to_string();
    }

    fn on_hello(&self, h: &WorkerHello) {
        if h.model.is_empty() || h.revision.is_empty() {
            let missing = h.missing.join(",");
            AgentLog::warn(&format!(
                "worker has no embedder (device {}, missing [{missing}]); embedding space kept",
                h.device
            ));
            return;
        }
        if let Some(ref dim_fn) = self.inner.index_dim {
            let want = dim_fn();
            if h.dim != want {
                AgentLog::warn(&format!(
                    "worker embeds at dim {}, not index.dim {want}; embedding space kept",
                    h.dim
                ));
                let now = tick_count64();
                let prev = self.inner.dim_restart_ticks.load(Ordering::Acquire);
                if now.saturating_sub(prev) > 600_000 {
                    self.inner.dim_restart_ticks.store(now, Ordering::Release);
                    self.inner.worker.supervisor().request_restart("index.dim");
                }
                return;
            }
        }
        {
            let mut sp = self.inner.space.lock().unwrap_or_else(|e| e.into_inner());
            let info = EmbeddingInfo {
                model_id: h.model.clone(),
                revision: h.revision.clone(),
                dim: h.dim,
            };
            if sp.space.as_ref() != Some(&info) {
                sp.space = Some(info);
                sp.space_store_id = None;
            }
        }
        self.inner
            .space_refused_logged
            .store(false, Ordering::Release);
        {
            let mut w = self.inner.wake.lock().unwrap_or_else(|e| e.into_inner());
            w.hello_seq += 1;
            w.backing_off = false;
            w.signaled = true;
            self.inner.wake_cv.notify_one();
        }
    }

    fn ensure_space(&self, store: &Arc<dyn EmbeddingStore>) {
        let mut sp = self.inner.space.lock().unwrap_or_else(|e| e.into_inner());
        let Some(ref info) = sp.space else { return };
        let id = store.store_identity();
        if sp.space_store_id == Some(id) {
            return;
        }
        match store.ensure_embedding_space(info) {
            Ok(()) => sp.space_store_id = Some(id),
            Err(e) => AgentLog::warn_err("EnsureEmbeddingSpace", &e),
        }
    }

    fn has_accepted_space(&self) -> bool {
        self.inner
            .space
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .space
            .is_some()
    }

    fn note_attempt(&self, id: &str) -> i32 {
        let mut a = self.inner.attempts.lock().unwrap_or_else(|e| e.into_inner());
        let n = a.get(id).copied().unwrap_or(0) + 1;
        a.insert(id.to_string(), n);
        n
    }

    fn settled(&self, id: &str) {
        self.inner
            .attempts
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(id);
    }

    fn is_backoff_decision(d: &str) -> bool {
        matches!(
            d,
            "battery"
                | "fullscreen"
                | "cpu"
                | "gpu"
                | "model-missing"
                | "space-refused"
                | "worker-error"
                | "poison-job"
                | "worker-unavailable"
        )
    }

    fn run_loop(&self) {
        let ct = self.inner.cts.token();
        while !ct.is_cancelled() {
            {
                let mut w = self.inner.wake.lock().unwrap_or_else(|e| e.into_inner());
                w.step_hello_seq = w.hello_seq;
                w.backing_off = false;
                w.signaled = false;
            }
            let wait = self.step(&ct);
            if ct.is_cancelled() {
                break;
            }
            if !wait.is_zero() {
                let mut wake_now = false;
                {
                    let mut w = self.inner.wake.lock().unwrap_or_else(|e| e.into_inner());
                    let dec = self.last_decision();
                    if Self::is_backoff_decision(&dec) {
                        if w.hello_seq != w.step_hello_seq {
                            wake_now = true;
                        } else {
                            w.backing_off = true;
                            w.signaled = false;
                        }
                    }
                }
                if !wake_now {
                    let w = self.inner.wake.lock().unwrap_or_else(|e| e.into_inner());
                    if !w.signaled && !ct.is_cancelled() {
                        let _ = self.inner.wake_cv.wait_timeout(w, wait);
                    }
                }
            }
        }
    }

    /// Runs one scheduler step (public for unit tests matching `StepAsync`).
    pub fn step(&self, ct: &CancelToken) -> Duration {
        let store = (self.inner.store)();
        let Some(store) = store else {
            self.set_decision("no-store");
            return Duration::from_secs(5);
        };
        if (self.inner.paused)() {
            self.set_decision("paused");
            return Duration::from_secs(5);
        }
        self.ensure_space(&store);
        let s = (self.inner.settings)();
        let have_work = !self
            .inner
            .qstate
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .queue
            .is_empty();
        if !have_work {
            let jobs = match store.take_pending_embeddings(32) {
                Ok(j) => j,
                Err(e) => {
                    AgentLog::warn_err("TakePendingEmbeddings", &e);
                    return Duration::from_secs(10);
                }
            };
            if jobs.is_empty() {
                self.set_decision("empty");
                return Duration::from_secs(30);
            }
            let mut q = self.inner.qstate.lock().unwrap_or_else(|e| e.into_inner());
            q.queue.extend(jobs);
        }

        let decision = Governance::decide((self.inner.probe)(), &s.governance, &s.capture);
        self.set_decision(decision.reason);
        if !decision.run {
            return decision.next_check;
        }

        if !self.has_accepted_space() {
            if let Err(e) = self.inner.worker.prewarm(ct) {
                if ct.is_cancelled() {
                    return Duration::ZERO;
                }
                AgentLog::debug(&format!("scheduler deferred: {}", e.message()));
                self.set_decision("worker-unavailable");
                return Duration::from_secs(15);
            }
            self.ensure_space(&store);
            let has_space = self.has_accepted_space();
            {
                let mut w = self.inner.wake.lock().unwrap_or_else(|e| e.into_inner());
                w.step_hello_seq = w.hello_seq;
            }
            if !has_space {
                if !self
                    .inner
                    .space_refused_logged
                    .swap(true, Ordering::AcqRel)
                {
                    AgentLog::warn("indexing waits: no embedding space accepted yet");
                }
                self.set_decision("space-refused");
                return Duration::from_secs(60);
            }
        } else {
            let mut w = self.inner.wake.lock().unwrap_or_else(|e| e.into_inner());
            w.step_hello_seq = w.hello_seq;
        }

        for _ in 0..decision.batch_size {
            let mut job = {
                let mut q = self.inner.qstate.lock().unwrap_or_else(|e| e.into_inner());
                let Some(j) = q.queue.pop_front() else { break };
                q.in_flight = Some(j.id.clone());
                q.in_flight_dropped = false;
                j
            };

            if self.dropped_in_flight(&job.id) {
                self.finish_in_flight(&mut job, false);
                continue;
            }

            let outcome = self.process_job(&store, &job, &s, ct);
            match outcome {
                Ok(()) => {
                    self.settled(&job.id);
                    self.finish_in_flight(&mut job, false);
                }
                Err(JobErr::SpaceRefused(msg)) => {
                    let requeued = self.requeue(job);
                    if !self
                        .inner
                        .space_refused_logged
                        .swap(true, Ordering::AcqRel)
                    {
                        AgentLog::warn(&format!("indexing waits: {msg}"));
                    }
                    let _ = requeued;
                    self.set_decision("space-refused");
                    return Duration::from_secs(60);
                }
                Err(JobErr::Worker(ref e)) if e.model_missing() => {
                    let msg = e.message().to_string();
                    let _ = self.requeue(job);
                    AgentLog::warn(&format!("indexing waits: {msg} (retry in 10 min)"));
                    self.set_decision("model-missing");
                    return Duration::from_secs(600);
                }
                Err(JobErr::Worker(ref e)) if Self::is_transient(e) => {
                    let n = self.note_attempt(&job.id);
                    if n < Self::MAX_TRANSIENT_ATTEMPTS {
                        let id = job.id.clone();
                        let msg = e.message().to_string();
                        let _ = self.requeue(job);
                        AgentLog::warn(&format!(
                            "embedding {id}: {msg} (attempt {n} of {}, will retry)",
                            Self::MAX_TRANSIENT_ATTEMPTS
                        ));
                        self.set_decision("worker-error");
                        let secs = 300u64.min(15u64 << (n - 1).min(4) as u32);
                        return Duration::from_secs(secs);
                    } else {
                        self.settled(&job.id);
                        let _ = store.fail_embedding(&job.id, e.message());
                        AgentLog::debug(&format!(
                            "embedding failed for {}: {}",
                            job.id,
                            e.message()
                        ));
                        self.finish_in_flight(&mut job, false);
                    }
                }
                Err(JobErr::Worker(WorkerError::Reply(msg))) => {
                    self.settled(&job.id);
                    let _ = store.fail_embedding(&job.id, &msg);
                    AgentLog::debug(&format!("embedding failed for {}: {msg}", job.id));
                    self.finish_in_flight(&mut job, false);
                }
                Err(JobErr::Worker(ref e)) if Self::is_crash(e) => {
                    let n = self.note_attempt(&job.id);
                    if n >= Self::MAX_CRASH_ATTEMPTS {
                        self.settled(&job.id);
                        let _ = store.fail_embedding(&job.id, "worker crashed");
                        AgentLog::warn(&format!(
                            "embedding {}: the worker crashed on it {} times; set aside",
                            job.id,
                            Self::MAX_CRASH_ATTEMPTS
                        ));
                        self.finish_in_flight(&mut job, false);
                        self.set_decision("poison-job");
                        return Duration::from_secs(15);
                    } else {
                        let _ = self.requeue(job);
                        AgentLog::debug(&format!("scheduler deferred: {}", e.message()));
                        self.set_decision("worker-unavailable");
                        return Duration::from_secs(15);
                    }
                }
                Err(JobErr::Worker(WorkerError::Cancelled)) if ct.is_cancelled() => {
                    self.finish_in_flight(&mut job, false);
                    return Duration::ZERO;
                }
                Err(JobErr::Worker(e)) => {
                    let msg = e.message().to_string();
                    let _ = self.requeue(job);
                    AgentLog::debug(&format!("scheduler deferred: {msg}"));
                    self.set_decision("worker-unavailable");
                    return Duration::from_secs(15);
                }
                Err(JobErr::Local(msg)) => {
                    self.settled(&job.id);
                    let _ = store.fail_embedding(&job.id, &msg);
                    AgentLog::warn_err(&format!("embedding job {}", job.id), &msg);
                    self.finish_in_flight(&mut job, false);
                }
            }
        }
        decision.next_check
    }

    fn dropped_in_flight(&self, id: &str) -> bool {
        let dropped = {
            let q = self.inner.qstate.lock().unwrap_or_else(|e| e.into_inner());
            q.in_flight.as_deref() == Some(id) && q.in_flight_dropped
        };
        if dropped {
            self.settled(id);
            AgentLog::debug(&format!("embedding {id}: purged while in flight, dropped"));
        }
        dropped
    }

    fn finish_in_flight(&self, job: &mut PendingJob, requeued: bool) {
        {
            let mut q = self.inner.qstate.lock().unwrap_or_else(|e| e.into_inner());
            if q.in_flight.as_deref() == Some(job.id.as_str()) {
                q.in_flight = None;
            }
        }
        if !requeued && let Some(ref mut img) = job.image {
            img.zeroize();
        }
    }

    fn requeue(&self, mut job: PendingJob) -> bool {
        if self.dropped_in_flight(&job.id) {
            self.finish_in_flight(&mut job, false);
            return false;
        }
        let mut q = self.inner.qstate.lock().unwrap_or_else(|e| e.into_inner());
        if q.in_flight.as_deref() == Some(job.id.as_str()) {
            q.in_flight = None;
        }
        q.queue.push_front(job);
        true
    }

    fn process_job(
        &self,
        store: &Arc<dyn EmbeddingStore>,
        job: &PendingJob,
        s: &Settings,
        ct: &CancelToken,
    ) -> Result<(), JobErr> {
        if job.kind != CaptureKind::Audio {
            let (Some(img), Some(mime)) = (job.image.as_deref(), job.image_mime.as_deref()) else {
                let _ = store.fail_embedding(&job.id, "no image");
                return Ok(());
            };
            let nsfw = if s.privacy.nsfw_filter {
                Some(s.privacy.nsfw_threshold)
            } else {
                None
            };
            let detector = if s.privacy.nsfw_filter && s.privacy.nsfw_detector {
                Some(s.privacy.nsfw_detector_threshold)
            } else {
                None
            };
            let r = if commonly_decodable(mime) {
                self.inner
                    .worker
                    .embed_image(img, mime, nsfw, s.privacy.nsfw_grid, detector, ct)
                    .map_err(JobErr::Worker)?
            } else {
                let (mut px, w, h) = self
                    .inner
                    .decoder
                    .decode_bgra(img, mime)
                    .map_err(JobErr::Local)?;
                let res = self.inner.worker.embed_bgra(
                    &px,
                    (w, h),
                    nsfw,
                    s.privacy.nsfw_grid,
                    detector,
                    ct,
                );
                px.zeroize();
                res.map_err(JobErr::Worker)?
            };
            if r.dropped_nsfw {
                let _ = store.drop_capture(&job.id, "nsfw");
                self.inner.dropped.fetch_add(1, Ordering::AcqRel);
                return Ok(());
            }
            let vec = r
                .vector
                .ok_or_else(|| JobErr::Local("missing vector".to_string()))?;
            let space = self.space_for(vec.len() as i32, r.model.as_deref(), r.revision.as_deref())?;
            self.ensure_space(store);
            store
                .set_embedding(&job.id, &vec, &space)
                .map_err(JobErr::Local)?;
            self.inner.processed.fetch_add(1, Ordering::AcqRel);
        } else {
            let text = job.text.as_deref().unwrap_or("");
            if text.is_empty() {
                let _ = store.fail_embedding(&job.id, "no text");
                return Ok(());
            }
            let t = self
                .inner
                .worker
                .embed_text_labelled(text, "document", ct)
                .map_err(JobErr::Worker)?;
            let space =
                self.space_for(t.vector.len() as i32, t.model.as_deref(), t.revision.as_deref())?;
            self.ensure_space(store);
            store
                .set_embedding(&job.id, &t.vector, &space)
                .map_err(JobErr::Local)?;
            self.inner.processed.fetch_add(1, Ordering::AcqRel);
        }
        Ok(())
    }

    fn space_for(
        &self,
        dim: i32,
        model: Option<&str>,
        revision: Option<&str>,
    ) -> Result<EmbeddingInfo, JobErr> {
        let space = self
            .inner
            .space
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .space
            .clone()
            .ok_or_else(|| JobErr::SpaceRefused("no embedding space accepted yet".to_string()))?;
        if space.dim != dim {
            return Err(JobErr::SpaceRefused(format!(
                "vector dim {dim} is not the index dim {}",
                space.dim
            )));
        }
        if let Some(m) = model {
            let rev = revision.unwrap_or("");
            if m != space.model_id || rev != space.revision {
                return Err(JobErr::SpaceRefused(format!(
                    "vector from {m} rev {rev}, index is {} rev {}",
                    space.model_id, space.revision
                )));
            }
        }
        Ok(space)
    }

    pub fn stop(&self) {
        self.inner.cts.cancel();
        {
            let mut w = self.inner.wake.lock().unwrap_or_else(|e| e.into_inner());
            w.signaled = true;
            self.inner.wake_cv.notify_all();
        }
        if let Some(h) = self
            .loop_thread
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
            && h.thread().id() != thread::current().id()
        {
            let _ = h.join();
        }
        self.clear();
        let lid = self.inner.hello_listener_id.swap(0, Ordering::AcqRel);
        if lid != 0 {
            self.inner
                .worker
                .supervisor()
                .remove_hello_listener(lid as u64);
        }
    }
}

impl Drop for EmbeddingScheduler {
    fn drop(&mut self) {
        self.stop();
    }
}

enum JobErr {
    Worker(WorkerError),
    SpaceRefused(String),
    Local(String),
}
