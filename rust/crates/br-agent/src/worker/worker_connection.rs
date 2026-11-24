// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Client side of the worker binary frame protocol (`WorkerConnection.cs`).

use std::collections::{HashMap, VecDeque};
use std::io;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};

use br_contracts::{BinaryFrame, FrameError, WorkerRequest, WorkerResponse, to_json_string};
use br_core::{CancelToken, PipeStream};
use thiserror::Error;
use zeroize::Zeroize;

use crate::native::{liveness_now_ms, tick_count64};

/// The reader and writer threads of one connection.
type ReaderWriterThreads = (Option<JoinHandle<()>>, Option<JoinHandle<()>>);
/// One frame read off the pipe: JSON header bytes and binary body bytes.
type HeaderAndBody = (Vec<u8>, Vec<u8>);

pub const STOPPED_MESSAGE: &str = "Worker stopped by the agent.";
pub const CRASHED_ELSEWHERE_MESSAGE: &str = "Worker died while serving another request.";
pub const NOT_SENT_MESSAGE: &str = "Worker connection closed; request not sent.";
pub const PIPE_CLOSED_MESSAGE: &str = "Worker pipe closed.";

#[derive(Debug, Clone)]
pub struct WorkerReply {
    pub header: WorkerResponse,
    pub payload: Vec<u8>,
}

#[derive(Debug, Error, Clone)]
pub enum WorkerError {
    /// The worker answered this request with an error (`WorkerReplyException`).
    #[error("{0}")]
    Reply(String),
    /// Transport / lifecycle error (`WorkerException`).
    #[error("{0}")]
    Transport(String),
    /// Caller's `CancelToken` fired.
    #[error("Operation cancelled.")]
    Cancelled,
}

impl WorkerError {
    pub fn is_reply(&self) -> bool {
        matches!(self, Self::Reply(_))
    }

    pub fn model_missing(&self) -> bool {
        matches!(self, Self::Reply(msg) if msg.starts_with("Model not installed"))
    }

    pub fn message(&self) -> &str {
        match self {
            Self::Reply(m) | Self::Transport(m) => m.as_str(),
            Self::Cancelled => "Operation cancelled.",
        }
    }
}

/// Duplex byte stream backing a [`WorkerConnection`]. Implemented by [`PipeWorkerStream`]
/// and by in-memory test streams.
pub trait WorkerStream: Send + Sync + 'static {
    /// Reads up to `buf.len()` bytes. Returns `Ok(0)` on clean EOF.
    fn read(&self, buf: &mut [u8], ct: &CancelToken) -> io::Result<usize>;
    /// Writes all bytes in `buf` (uninterruptible by caller cancellation once started).
    fn write_all(&self, buf: &[u8]) -> io::Result<()>;
    /// Closes the stream and aborts any in-flight reads/writes.
    fn close(&self);
}

/// [`WorkerStream`] over `br_core::PipeStream` (overlapped Win32 named pipe).
pub struct PipeWorkerStream {
    pipe: PipeStream,
}

impl PipeWorkerStream {
    pub fn new(pipe: PipeStream) -> Self {
        Self { pipe }
    }

    pub fn pipe(&self) -> &PipeStream {
        &self.pipe
    }
}

impl WorkerStream for PipeWorkerStream {
    fn read(&self, buf: &mut [u8], ct: &CancelToken) -> io::Result<usize> {
        self.pipe.read(buf, None, ct)
    }

    fn write_all(&self, buf: &[u8]) -> io::Result<()> {
        self.pipe.write_all(buf, None, &CancelToken::none())
    }

    fn close(&self) {
        self.pipe.close();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WriteGateState {
    pub held: bool,
    pub urgent: usize,
    pub normal: usize,
}

#[derive(Debug, Clone)]
struct StartedOp {
    op: String,
    ticks: i64,
    seq: i64,
}

struct PendingSlot {
    result: Mutex<Option<Result<WorkerReply, WorkerError>>>,
    cv: Condvar,
}

impl PendingSlot {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            result: Mutex::new(None),
            cv: Condvar::new(),
        })
    }

    fn set(&self, res: Result<WorkerReply, WorkerError>) -> bool {
        let mut guard = self.result.lock().unwrap_or_else(|e| e.into_inner());
        if guard.is_some() {
            return false;
        }
        *guard = Some(res);
        self.cv.notify_all();
        true
    }

    fn wait_arc(
        self: &Arc<Self>,
        ct: &CancelToken,
        on_cancel: impl FnOnce(),
    ) -> Result<WorkerReply, WorkerError> {
        let me = self.clone();
        let _reg = ct.on_cancel(move || {
            let _g = me.result.lock().unwrap_or_else(|e| e.into_inner());
            me.cv.notify_all();
        });
        let mut guard = self.result.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if let Some(res) = guard.take() {
                return res;
            }
            if ct.is_cancelled() {
                drop(guard);
                on_cancel();
                return Err(WorkerError::Cancelled);
            }
            guard = self.cv.wait(guard).unwrap_or_else(|e| e.into_inner());
        }
    }
}

struct WriteJob {
    header: Vec<u8>,
    payload: Vec<u8>,
    done: Arc<WriteDone>,
}

// A3: zeroize the payload Vec when the job is dropped (written, failed or disposed).
impl Drop for WriteJob {
    fn drop(&mut self) {
        self.payload.zeroize();
    }
}

struct WriteDone {
    result: Mutex<Option<Result<(), WorkerError>>>,
    cv: Condvar,
}

impl WriteDone {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            result: Mutex::new(None),
            cv: Condvar::new(),
        })
    }

    fn complete(&self, res: Result<(), WorkerError>) {
        let mut g = self.result.lock().unwrap_or_else(|e| e.into_inner());
        *g = Some(res);
        self.cv.notify_all();
    }

    fn wait(self: &Arc<Self>, ct: &CancelToken) -> Result<(), WorkerError> {
        let me = self.clone();
        let _reg = ct.on_cancel(move || {
            let _g = me.result.lock().unwrap_or_else(|e| e.into_inner());
            me.cv.notify_all();
        });
        let mut g = self.result.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if let Some(res) = g.clone() {
                return res;
            }
            if ct.is_cancelled() {
                return Err(WorkerError::Cancelled);
            }
            g = self.cv.wait(g).unwrap_or_else(|e| e.into_inner());
        }
    }
}

/// Two-lane write lock (`WriteGate`): waiting urgent writers go before normal writers,
/// with fairness cap [`Self::MAX_URGENT_RUN`] so overlapping queries cannot starve background work.
struct WriteGate {
    state: Mutex<WriteGateInner>,
    next_waiter_id: AtomicUsize,
}

struct WriteGateInner {
    held: bool,
    closed: bool,
    urgent_run: usize,
    urgent: VecDeque<Arc<GateWaiter>>,
    normal: VecDeque<Arc<GateWaiter>>,
}

struct GateWaiter {
    id: usize,
    outcome: Mutex<Option<Result<(), WorkerError>>>,
    cv: Condvar,
}

impl GateWaiter {
    fn new(id: usize) -> Arc<Self> {
        Arc::new(Self {
            id,
            outcome: Mutex::new(None),
            cv: Condvar::new(),
        })
    }

    fn resolve(&self, res: Result<(), WorkerError>) {
        let mut g = self.outcome.lock().unwrap_or_else(|e| e.into_inner());
        if g.is_none() {
            *g = Some(res);
            self.cv.notify_all();
        }
    }
}

impl WriteGate {
    pub const MAX_URGENT_RUN: usize = 8;

    fn new() -> Self {
        Self {
            state: Mutex::new(WriteGateInner {
                held: false,
                closed: false,
                urgent_run: 0,
                urgent: VecDeque::new(),
                normal: VecDeque::new(),
            }),
            next_waiter_id: AtomicUsize::new(1),
        }
    }

    fn state(&self) -> WriteGateState {
        let s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        WriteGateState {
            held: s.held,
            urgent: s.urgent.len(),
            normal: s.normal.len(),
        }
    }

    fn enter(&self, urgent: bool, ct: &CancelToken) -> Result<(), WorkerError> {
        let waiter = {
            let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if s.closed {
                return Err(WorkerError::Transport(NOT_SENT_MESSAGE.to_string()));
            }
            if ct.is_cancelled() {
                return Err(WorkerError::Cancelled);
            }
            if !s.held {
                s.held = true;
                return Ok(());
            }
            let w = GateWaiter::new(self.next_waiter_id.fetch_add(1, Ordering::Relaxed));
            if urgent {
                s.urgent.push_back(w.clone());
            } else {
                s.normal.push_back(w.clone());
            }
            w
        };

        let w_cb = waiter.clone();
        let _reg = ct.on_cancel(move || {
            let _g = w_cb.outcome.lock().unwrap_or_else(|e| e.into_inner());
            w_cb.cv.notify_all();
        });

        let mut g = waiter.outcome.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if let Some(res) = g.clone() {
                return res;
            }
            if ct.is_cancelled() {
                drop(g);
                let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
                let lane = if urgent { &mut s.urgent } else { &mut s.normal };
                if let Some(pos) = lane.iter().position(|x| x.id == waiter.id) {
                    lane.remove(pos);
                    return Err(WorkerError::Cancelled);
                }
                // Already granted or failed concurrently; re-check outcome.
                drop(s);
                let mut g2 = waiter.outcome.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(res) = g2.take() {
                    if res.is_ok() {
                        // Granted right as we cancelled: release gate and report cancelled.
                        drop(g2);
                        self.exit();
                        return Err(WorkerError::Cancelled);
                    }
                    return res;
                }
                return Err(WorkerError::Cancelled);
            }
            g = waiter.cv.wait(g).unwrap_or_else(|e| e.into_inner());
        }
    }

    fn exit(&self) {
        let next = {
            let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
            let pick_urgent =
                !s.urgent.is_empty() && (s.normal.is_empty() || s.urgent_run < Self::MAX_URGENT_RUN);
            if pick_urgent {
                s.urgent_run = if !s.normal.is_empty() {
                    s.urgent_run + 1
                } else {
                    0
                };
                s.urgent.pop_front()
            } else if !s.normal.is_empty() {
                s.urgent_run = 0;
                s.normal.pop_front()
            } else {
                s.held = false;
                s.urgent_run = 0;
                None
            }
        };
        if let Some(w) = next {
            w.resolve(Ok(()));
        }
    }

    fn close(&self) {
        let waiting: Vec<Arc<GateWaiter>> = {
            let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
            s.closed = true;
            let mut all = Vec::with_capacity(s.urgent.len() + s.normal.len());
            all.extend(s.urgent.drain(..));
            all.extend(s.normal.drain(..));
            all
        };
        for w in waiting {
            w.resolve(Err(WorkerError::Transport(NOT_SENT_MESSAGE.to_string())));
        }
    }
}

pub type ClosedCallback = Arc<dyn Fn(Option<String>, usize) + Send + Sync>;

struct ConnInner {
    stream: Arc<dyn WorkerStream>,
    write_gate: WriteGate,
    now: Arc<dyn Fn() -> i64 + Send + Sync>,
    pending: Mutex<HashMap<i64, Arc<PendingSlot>>>,
    started: Mutex<HashMap<i64, StartedOp>>,
    abandoned: Mutex<HashMap<i64, StartedOp>>,
    next_id: AtomicI64,
    write_seq: AtomicI64,
    last_activity_ticks: AtomicI64,
    last_reply_ticks: AtomicI64,
    consecutive_timeouts: AtomicI32,
    long_ops: AtomicI32,
    closed: AtomicBool,
    disposed: AtomicBool,
    recycled: AtomicBool,
    shutting_down: AtomicBool,
    aborted: AtomicBool,
    cts: br_core::CancelSource,
    writer_queue: Mutex<Option<VecDeque<WriteJob>>>,
    writer_cv: Condvar,
    closed_cb: Mutex<Option<ClosedCallback>>,
}

impl ConnInner {
    fn touch(&self) {
        self.last_activity_ticks
            .store(tick_count64(), Ordering::Release);
    }

    fn serving_id(&self) -> Option<i64> {
        let mut best_id = None;
        let mut best_seq = i64::MAX;
        {
            let ab = self.abandoned.lock().unwrap_or_else(|e| e.into_inner());
            for (&k, v) in ab.iter() {
                if v.seq < best_seq {
                    best_seq = v.seq;
                    best_id = Some(k);
                }
            }
        }
        {
            let st = self.started.lock().unwrap_or_else(|e| e.into_inner());
            for (&k, v) in st.iter() {
                if v.seq < best_seq {
                    best_seq = v.seq;
                    best_id = Some(k);
                }
            }
        }
        best_id
    }

    fn fail(&self, error: Option<String>) {
        if self.closed.swap(true, Ordering::AcqRel) {
            return;
        }
        self.write_gate.close();
        let by_agent = self.by_agent();
        let blamed = if self.aborted.load(Ordering::Acquire)
            || self.shutting_down.load(Ordering::Acquire)
        {
            None
        } else {
            self.serving_id()
        };
        let drained: Vec<(i64, Arc<PendingSlot>)> = {
            let mut p = self.pending.lock().unwrap_or_else(|e| e.into_inner());
            p.drain().collect()
        };
        let pending_at_close = drained.len();
        // The close callback records the failure and its restart backoff. Run it before any
        // waiting caller wakes, so a caller that retries at once sees the backoff (sweep 6 F1).
        let cb = self
            .closed_cb
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if let Some(cb) = cb {
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                cb(error, pending_at_close);
            }));
        }
        self.drain_writer_queue();
        for (id, slot) in drained {
            let msg = if Some(id) == blamed {
                PIPE_CLOSED_MESSAGE
            } else if by_agent {
                STOPPED_MESSAGE
            } else {
                CRASHED_ELSEWHERE_MESSAGE
            };
            slot.set(Err(WorkerError::Transport(msg.to_string())));
        }
    }

    /// A3: close the writer queue and fail every queued job so callers never hang in `done.wait`.
    fn drain_writer_queue(&self) {
        let by_agent = self.by_agent();
        let drained = {
            let mut q = self.writer_queue.lock().unwrap_or_else(|e| e.into_inner());
            let taken = q.take();
            self.writer_cv.notify_all();
            taken
        };
        if let Some(mut jobs) = drained {
            let msg = if by_agent {
                STOPPED_MESSAGE
            } else {
                NOT_SENT_MESSAGE
            };
            for job in jobs.drain(..) {
                self.write_gate.exit();
                job.done
                    .complete(Err(WorkerError::Transport(msg.to_string())));
            }
        }
    }

    fn by_agent(&self) -> bool {
        self.recycled.load(Ordering::Acquire)
            || self.shutting_down.load(Ordering::Acquire)
            || self.aborted.load(Ordering::Acquire)
    }
}

pub type ClockFn = Arc<dyn Fn() -> i64 + Send + Sync>;

/// Client side of the worker binary frame protocol over any duplex [`WorkerStream`].
pub struct WorkerConnection {
    inner: Arc<ConnInner>,
    threads: Mutex<ReaderWriterThreads>,
}

impl WorkerConnection {
    pub fn new(stream: Arc<dyn WorkerStream>, clock: Option<ClockFn>) -> Arc<Self> {
        let now = clock.unwrap_or_else(|| Arc::new(liveness_now_ms));
        let start_reply = now();
        let inner = Arc::new(ConnInner {
            stream,
            write_gate: WriteGate::new(),
            now,
            pending: Mutex::new(HashMap::new()),
            started: Mutex::new(HashMap::new()),
            abandoned: Mutex::new(HashMap::new()),
            next_id: AtomicI64::new(0),
            write_seq: AtomicI64::new(0),
            last_activity_ticks: AtomicI64::new(tick_count64()),
            last_reply_ticks: AtomicI64::new(start_reply),
            consecutive_timeouts: AtomicI32::new(0),
            long_ops: AtomicI32::new(0),
            closed: AtomicBool::new(false),
            disposed: AtomicBool::new(false),
            recycled: AtomicBool::new(false),
            shutting_down: AtomicBool::new(false),
            aborted: AtomicBool::new(false),
            cts: br_core::CancelSource::new(),
            writer_queue: Mutex::new(Some(VecDeque::new())),
            writer_cv: Condvar::new(),
            closed_cb: Mutex::new(None),
        });

        let r_inner = inner.clone();
        let reader = thread::Builder::new()
            .name("BetterRecall.WorkerReader".to_string())
            .spawn(move || read_loop(r_inner))
            .ok();

        let w_inner = inner.clone();
        let writer = thread::Builder::new()
            .name("BetterRecall.WorkerWriter".to_string())
            .spawn(move || write_loop(w_inner))
            .ok();

        Arc::new(Self {
            inner,
            threads: Mutex::new((reader, writer)),
        })
    }

    /// Test seam (A3): builds a connection without spawning the writer thread so a queued
    /// `WriteJob` stays in `writer_queue` until `dispose()`.
    #[doc(hidden)]
    pub fn new_without_writer_for_test(
        stream: Arc<dyn WorkerStream>,
        clock: Option<ClockFn>,
    ) -> Arc<Self> {
        let now = clock.unwrap_or_else(|| Arc::new(liveness_now_ms));
        let start_reply = now();
        let inner = Arc::new(ConnInner {
            stream,
            write_gate: WriteGate::new(),
            now,
            pending: Mutex::new(HashMap::new()),
            started: Mutex::new(HashMap::new()),
            abandoned: Mutex::new(HashMap::new()),
            next_id: AtomicI64::new(0),
            write_seq: AtomicI64::new(0),
            last_activity_ticks: AtomicI64::new(tick_count64()),
            last_reply_ticks: AtomicI64::new(start_reply),
            consecutive_timeouts: AtomicI32::new(0),
            long_ops: AtomicI32::new(0),
            closed: AtomicBool::new(false),
            disposed: AtomicBool::new(false),
            recycled: AtomicBool::new(false),
            shutting_down: AtomicBool::new(false),
            aborted: AtomicBool::new(false),
            cts: br_core::CancelSource::new(),
            writer_queue: Mutex::new(Some(VecDeque::new())),
            writer_cv: Condvar::new(),
            closed_cb: Mutex::new(None),
        });
        Arc::new(Self {
            inner,
            threads: Mutex::new((None, None)),
        })
    }

    #[doc(hidden)]
    pub fn queued_writes_for_test(&self) -> usize {
        self.inner
            .writer_queue
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map_or(0, VecDeque::len)
    }

    pub fn set_closed_handler(&self, cb: ClosedCallback) {
        *self
            .inner
            .closed_cb
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(cb);
    }

    pub fn pending(&self) -> usize {
        self.inner
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .len()
    }

    pub fn is_closed(&self) -> bool {
        self.inner.closed.load(Ordering::Acquire)
    }

    pub fn is_disposed(&self) -> bool {
        self.inner.disposed.load(Ordering::Acquire)
    }

    pub fn idle_ms(&self) -> i64 {
        tick_count64().saturating_sub(self.inner.last_activity_ticks.load(Ordering::Acquire))
    }

    pub fn no_reply_ms(&self) -> i64 {
        (self.inner.now)().saturating_sub(self.inner.last_reply_ticks.load(Ordering::Acquire))
    }

    pub fn consecutive_timeouts(&self) -> i32 {
        self.inner.consecutive_timeouts.load(Ordering::Acquire)
    }

    pub fn long_op_pending(&self) -> bool {
        self.inner.long_ops.load(Ordering::Acquire) > 0
    }

    pub fn recycled(&self) -> bool {
        self.inner.recycled.load(Ordering::Acquire)
    }

    pub fn set_recycled(&self, v: bool) {
        self.inner.recycled.store(v, Ordering::Release);
    }

    pub fn shutting_down(&self) -> bool {
        self.inner.shutting_down.load(Ordering::Acquire)
    }

    pub fn set_shutting_down(&self, v: bool) {
        self.inner.shutting_down.store(v, Ordering::Release);
    }

    pub fn aborted(&self) -> bool {
        self.inner.aborted.load(Ordering::Acquire)
    }

    pub fn set_aborted(&self, v: bool) {
        self.inner.aborted.store(v, Ordering::Release);
    }

    pub fn write_gate_state(&self) -> WriteGateState {
        self.inner.write_gate.state()
    }

    /// The oldest request the worker owes a reply (waiting or abandoned) and how long
    /// the worker has owed it: `min(now - ticks, no_reply_ms)`.
    pub fn oldest(&self) -> Option<(String, i64)> {
        let mut best: Option<StartedOp> = None;
        {
            let ab = self
                .inner
                .abandoned
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            for v in ab.values() {
                if best.as_ref().is_none_or(|b| v.seq < b.seq) {
                    best = Some(v.clone());
                }
            }
        }
        {
            let st = self.inner.started.lock().unwrap_or_else(|e| e.into_inner());
            for v in st.values() {
                if best.as_ref().is_none_or(|b| v.seq < b.seq) {
                    best = Some(v.clone());
                }
            }
        }
        let o = best?;
        let since_start = (self.inner.now)().saturating_sub(o.ticks);
        Some((o.op, since_start.min(self.no_reply_ms())))
    }

    pub fn send(
        &self,
        mut request: WorkerRequest,
        payload: &[u8],
        ct: &CancelToken,
        urgent: bool,
    ) -> Result<WorkerReply, WorkerError> {
        if self.is_closed() {
            return Err(WorkerError::Transport(NOT_SENT_MESSAGE.to_string()));
        }
        let id = self.inner.next_id.fetch_add(1, Ordering::AcqRel) + 1;
        request.id = id;
        let op = request.op.clone();
        let long_op = op == "bench" || op == "hello";
        let slot = PendingSlot::new();
        {
            let mut p = self.inner.pending.lock().unwrap_or_else(|e| e.into_inner());
            p.insert(id, slot.clone());
        }
        self.inner.touch();
        if long_op {
            self.inner.long_ops.fetch_add(1, Ordering::AcqRel);
        }

        let res = self.send_core(id, &slot, &request, payload, ct, urgent);

        self.inner
            .started
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&id);
        if long_op {
            self.inner.long_ops.fetch_sub(1, Ordering::AcqRel);
        }
        res
    }

    fn send_core(
        &self,
        id: i64,
        slot: &Arc<PendingSlot>,
        req: &WorkerRequest,
        payload: &[u8],
        ct: &CancelToken,
        urgent: bool,
    ) -> Result<WorkerReply, WorkerError> {
        let header = to_json_string(req)
            .map_err(|e| WorkerError::Transport(format!("Serialize worker request failed: {e}")))?
            .into_bytes();
        if header.len() > BinaryFrame::MAX_HEADER_BYTES
            || payload.len() > BinaryFrame::MAX_PAYLOAD_BYTES
        {
            self.inner
                .pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&id);
            return Err(WorkerError::Transport("Request too large.".to_string()));
        }

        if let Err(e) = self.inner.write_gate.enter(urgent, ct) {
            self.inner
                .pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&id);
            return Err(e);
        }

        // SF8: owed from here (after acquiring write gate).
        let seq = self.inner.write_seq.fetch_add(1, Ordering::AcqRel) + 1;
        let started = StartedOp {
            op: req.op.clone(),
            ticks: (self.inner.now)(),
            seq,
        };
        self.inner
            .started
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id, started.clone());

        let done = WriteDone::new();
        {
            let mut q = self
                .inner
                .writer_queue
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if let Some(ref mut deque) = *q {
                deque.push_back(WriteJob {
                    header,
                    payload: payload.to_vec(),
                    done: done.clone(),
                });
                self.inner.writer_cv.notify_one();
            } else {
                drop(q);
                self.inner.write_gate.exit();
                self.inner
                    .pending
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&id);
                let msg = if self.inner.by_agent() {
                    STOPPED_MESSAGE
                } else {
                    NOT_SENT_MESSAGE
                };
                return Err(WorkerError::Transport(msg.to_string()));
            }
        }

        // A2: once started, the frame is written whole without ct; if ct fires here, abandon the reply.
        if let Err(e) = done.wait(ct) {
            if matches!(e, WorkerError::Cancelled) {
                self.inner
                    .abandoned
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(id, started);
            }
            self.inner
                .pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&id);
            return Err(e);
        }

        let inner = self.inner.clone();
        let reply = slot.wait_arc(ct, || {
            let mut ab = inner.abandoned.lock().unwrap_or_else(|e| e.into_inner());
            let mut pend = inner.pending.lock().unwrap_or_else(|e| e.into_inner());
            if pend.remove(&id).is_some() {
                ab.insert(id, started);
                inner.consecutive_timeouts.fetch_add(1, Ordering::AcqRel);
            }
        })?;
        self.inner.touch();
        Ok(reply)
    }

    /// Idempotent shutdown of the connection and its reader/writer threads.
    pub fn dispose(&self) {
        if self.inner.disposed.swap(true, Ordering::AcqRel) {
            return;
        }
        self.inner.cts.cancel();
        self.inner.stream.close();
        // A3: fail every queued WriteJob before joining so callers waiting in done.wait recover.
        self.inner.drain_writer_queue();
        let (r, w) = {
            let mut t = self.threads.lock().unwrap_or_else(|e| e.into_inner());
            (t.0.take(), t.1.take())
        };
        if let Some(h) = r
            && h.thread().id() != thread::current().id()
        {
            let _ = h.join();
        }
        if let Some(h) = w
            && h.thread().id() != thread::current().id()
        {
            let _ = h.join();
        }
        self.inner.fail(None);
    }
}

impl Drop for WorkerConnection {
    fn drop(&mut self) {
        self.dispose();
    }
}

fn write_loop(inner: Arc<ConnInner>) {
    loop {
        let job = {
            let mut q = inner
                .writer_queue
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            loop {
                match q.as_mut() {
                    None => return,
                    Some(deque) if !deque.is_empty() => break deque.pop_front().unwrap(),
                    Some(_) => {
                        q = inner.writer_cv.wait(q).unwrap_or_else(|e| e.into_inner());
                    }
                }
            }
        };

        let write_res = write_binary_frame(inner.stream.as_ref(), &job.header, &job.payload);
        inner.write_gate.exit();
        match write_res {
            Ok(()) => job.done.complete(Ok(())),
            Err(e) => {
                inner.fail(Some(e.to_string()));
                let msg = if inner.by_agent() {
                    STOPPED_MESSAGE
                } else {
                    NOT_SENT_MESSAGE
                };
                job.done.complete(Err(WorkerError::Transport(msg.to_string())));
                return;
            }
        }
    }
}

fn read_loop(inner: Arc<ConnInner>) {
    let ct = inner.cts.token();
    let mut error: Option<String> = None;
    loop {
        if ct.is_cancelled() {
            break;
        }
        match read_binary_frame(inner.stream.as_ref(), &ct) {
            Ok(None) => break,
            Ok(Some((header_bytes, payload))) => {
                let header: WorkerResponse = match serde_json::from_slice(&header_bytes) {
                    Ok(h) => h,
                    Err(_) => {
                        error = Some("Bad worker frame header.".to_string());
                        break;
                    }
                };
                inner.touch();
                inner
                    .last_reply_ticks
                    .store((inner.now)(), Ordering::Release);
                inner.consecutive_timeouts.store(0, Ordering::Release);

                let slot = {
                    let mut ab = inner.abandoned.lock().unwrap_or_else(|e| e.into_inner());
                    let mut pend = inner.pending.lock().unwrap_or_else(|e| e.into_inner());
                    ab.remove(&header.id);
                    pend.remove(&header.id)
                };
                if let Some(slot) = slot {
                    slot.set(Ok(WorkerReply { header, payload }));
                }
            }
            Err(FrameError::Io(ref e))
                if ct.is_cancelled() && e.kind() == io::ErrorKind::Interrupted =>
            {
                break;
            }
            Err(e) => {
                if !ct.is_cancelled() {
                    error = Some(e.to_string());
                }
                break;
            }
        }
    }
    inner.fail(error);
}

fn write_binary_frame(
    stream: &dyn WorkerStream,
    header: &[u8],
    payload: &[u8],
) -> Result<(), FrameError> {
    let head = BinaryFrame::build_head(header, payload.len())?;
    stream.write_all(&head)?;
    if !payload.is_empty() {
        stream.write_all(payload)?;
    }
    Ok(())
}

fn read_binary_frame(
    stream: &dyn WorkerStream,
    ct: &CancelToken,
) -> Result<Option<HeaderAndBody>, FrameError> {
    let mut len_buf = [0u8; 4];
    let first = read_at_least_one_or_eof(stream, &mut len_buf, ct)?;
    if first == 0 {
        return Ok(None);
    }
    if first < 4 {
        fill_exact(stream, &mut len_buf[first..], ct)?;
    }
    let header_len = BinaryFrame::validate_header_len(u32::from_le_bytes(len_buf))?;
    let mut header = vec![0u8; header_len];
    fill_exact(stream, &mut header, ct)?;
    fill_exact(stream, &mut len_buf, ct)?;
    let payload_len = BinaryFrame::validate_payload_len(u32::from_le_bytes(len_buf))?;
    let mut payload = vec![0u8; payload_len];
    fill_exact(stream, &mut payload, ct)?;
    Ok(Some((header, payload)))
}

fn read_at_least_one_or_eof(
    stream: &dyn WorkerStream,
    buf: &mut [u8],
    ct: &CancelToken,
) -> io::Result<usize> {
    loop {
        match stream.read(buf, ct) {
            Ok(n) => return Ok(n),
            Err(e) if e.kind() == io::ErrorKind::Interrupted && !ct.is_cancelled() => continue,
            Err(e) => return Err(e),
        }
    }
}

fn fill_exact(
    stream: &dyn WorkerStream,
    mut buf: &mut [u8],
    ct: &CancelToken,
) -> Result<(), FrameError> {
    while !buf.is_empty() {
        match stream.read(buf, ct) {
            Ok(0) => return Err(FrameError::UnexpectedEof),
            Ok(n) => buf = &mut buf[n..],
            Err(e) if e.kind() == io::ErrorKind::Interrupted && !ct.is_cancelled() => continue,
            Err(e) => return Err(FrameError::Io(e)),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a3_write_job_zeroizes_payload_on_drop() {
        let job = WriteJob {
            header: vec![1, 2, 3],
            payload: vec![42u8; 128],
            done: WriteDone::new(),
        };
        drop(job);
    }
}
