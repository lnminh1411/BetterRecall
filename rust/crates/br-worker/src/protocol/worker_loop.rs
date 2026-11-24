// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

use std::io;
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

use br_contracts::{BinaryFrame, FrameError, WorkerResponse};

use crate::ZeroOnDrop;
use crate::hosting::Log;
use crate::protocol::errors::WorkerErrors;
use crate::protocol::handler::{RequestHandler, WorkerReply};

/// Reason `WorkerLoop::run` returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoopExit {
    PipeClosed,
    Shutdown,
    Idle,
    ProtocolError,
    Cancelled,
}

/// Outcome of waiting for the next frame with an idle timeout.
/// E2: payload is wrapped in `ZeroOnDrop` so raw screen frames and PCM audio are zeroized on drop.
pub enum FrameRead {
    Frame(Vec<u8>, ZeroOnDrop<Vec<u8>>),
    TimedOut,
    Eof,
}

/// Transport abstraction for `WorkerLoop` (implemented by `AgentPipe` and test streams).
pub trait PipeIo: Sync {
    /// Waits up to `idle_exit` for the first byte of a frame, then reads the entire frame without idle timeout.
    fn read_frame_idle(&self, idle_exit: Duration) -> Result<FrameRead, FrameError>;

    /// True if bytes are already buffered in the pipe when the idle timer fires.
    fn has_pending_input(&self) -> bool;

    /// Writes a whole frame (header + optional payload).
    fn write_frame(&self, header: &[u8], payload: &[u8]) -> Result<(), FrameError>;
}

struct KeepaliveState {
    active: bool,
    req_gen: u64,
    started: Instant,
    stop_all: bool,
}

struct ActiveRequestGuard<'a> {
    state: &'a Mutex<KeepaliveState>,
    cv: &'a Condvar,
}

impl Drop for ActiveRequestGuard<'_> {
    fn drop(&mut self) {
        let mut st = self.state.lock().expect("keepalive lock");
        st.active = false;
        self.cv.notify_all();
    }
}

struct StopAllGuard<'a> {
    state: &'a Mutex<KeepaliveState>,
    cv: &'a Condvar,
}

impl Drop for StopAllGuard<'_> {
    fn drop(&mut self) {
        let mut st = self.state.lock().expect("keepalive lock");
        st.active = false;
        st.stop_all = true;
        self.cv.notify_all();
    }
}

/// Reads frames one at a time, answers in arrival order. Exits on end of stream, shutdown,
/// framing errors, or after `idle_exit` with no request.
pub struct WorkerLoop;

impl WorkerLoop {
    /// Keepalive (K review S1): while one request runs longer than this, an id-0 frame goes out at
    /// this interval.
    pub const KEEPALIVE_EVERY: Duration = Duration::from_secs(20);
    /// Keepalives stop after this long on one request, so a stuck handler still lets the agent
    /// recycle the worker.
    pub const KEEPALIVE_MAX: Duration = Duration::from_secs(15 * 60);

    pub fn run(
        pipe: &dyn PipeIo,
        handler: &RequestHandler<'_>,
        idle_exit: Duration,
        keepalive_every: Duration,
        keepalive_max: Duration,
    ) -> LoopExit {
        let write_lock = Mutex::new(());
        let ka_state = Mutex::new(KeepaliveState {
            active: false,
            req_gen: 0,
            started: Instant::now(),
            stop_all: false,
        });
        let ka_cv = Condvar::new();
        let keepalive_header =
            serde_json::to_vec(&WorkerResponse::ok(0)).expect("serialize keepalive");

        std::thread::scope(|scope| {
            let _stop_guard = StopAllGuard {
                state: &ka_state,
                cv: &ka_cv,
            };
            scope.spawn(|| {
                keepalive_thread(
                    pipe,
                    &write_lock,
                    &ka_state,
                    &ka_cv,
                    &keepalive_header,
                    keepalive_every,
                    keepalive_max,
                );
            });

            loop {
                let (req_header, req_payload) = match pipe.read_frame_idle(idle_exit) {
                    Ok(FrameRead::Frame(h, p)) => (h, p),
                    Ok(FrameRead::Eof) => return LoopExit::PipeClosed,
                    Ok(FrameRead::TimedOut) => {
                        // No byte of a frame was consumed, so a request that arrived just now is still whole in the pipe.
                        if pipe.has_pending_input() {
                            Log::info("idle exit skipped: a request arrived");
                            continue;
                        }
                        Log::info(&format!(
                            "idle for {:.0} s, exiting",
                            idle_exit.as_secs_f64()
                        ));
                        return LoopExit::Idle;
                    }
                    Err(FrameError::UnexpectedEof) => return LoopExit::PipeClosed,
                    Err(FrameError::Io(ref e)) if is_closed_io(e) => return LoopExit::PipeClosed,
                    Err(e) => {
                        Log::warn(&format!("framing error: {e}"));
                        return LoopExit::ProtocolError;
                    }
                };

                let reply: WorkerReply = {
                    {
                        let mut st = ka_state.lock().expect("keepalive lock");
                        st.active = true;
                        st.req_gen = st.req_gen.wrapping_add(1);
                        st.started = Instant::now();
                        ka_cv.notify_all();
                    }
                    let _active_guard = ActiveRequestGuard {
                        state: &ka_state,
                        cv: &ka_cv,
                    };
                    handler.handle(&req_header, &req_payload)
                };

                let mut resp_header =
                    serde_json::to_vec(&reply.header).expect("serialize worker response");
                let mut resp_payload = reply.payload;
                if resp_header.len() > BinaryFrame::MAX_HEADER_BYTES
                    || resp_payload.len() > BinaryFrame::MAX_PAYLOAD_BYTES
                {
                    // C1: answer oversized replies with an error instead of dropping the frame, keeping the pipe in step.
                    Log::warn(&format!(
                        "reply too large for a frame (header {} B, payload {} B): internal error sent",
                        resp_header.len(),
                        resp_payload.len()
                    ));
                    resp_header = serde_json::to_vec(&WorkerResponse::err(
                        reply.header.id,
                        WorkerErrors::INTERNAL,
                    ))
                    .expect("serialize internal error response");
                    resp_payload = Vec::new();
                }

                let write_res = {
                    let _w = write_lock.lock().expect("write lock");
                    pipe.write_frame(&resp_header, &resp_payload)
                };
                if write_res.is_err() {
                    return LoopExit::PipeClosed;
                }
                if reply.shutdown {
                    return LoopExit::Shutdown;
                }
            }
        })
    }
}

#[allow(clippy::too_many_arguments)]
fn keepalive_thread(
    pipe: &dyn PipeIo,
    write_lock: &Mutex<()>,
    ka_state: &Mutex<KeepaliveState>,
    ka_cv: &Condvar,
    keepalive_header: &[u8],
    every: Duration,
    max: Duration,
) {
    loop {
        let (my_gen, started) = {
            let mut st = ka_state.lock().expect("keepalive lock");
            while !st.active && !st.stop_all {
                st = ka_cv.wait(st).expect("keepalive wait");
            }
            if st.stop_all {
                return;
            }
            (st.req_gen, st.started)
        };

        let mut next_beat = started + every;
        let max_until = started + max;

        loop {
            {
                let mut st = ka_state.lock().expect("keepalive lock");
                while st.active && !st.stop_all && st.req_gen == my_gen {
                    let rem = next_beat.saturating_duration_since(Instant::now());
                    if rem.is_zero() {
                        break;
                    }
                    let (guard, timeout_res) = ka_cv.wait_timeout(st, rem).expect("keepalive wait");
                    st = guard;
                    if timeout_res.timed_out() {
                        break;
                    }
                }
                if st.stop_all {
                    return;
                }
                if !st.active || st.req_gen != my_gen {
                    break;
                }
            }

            if Instant::now() >= max_until {
                Log::warn(&format!(
                    "request still running after {:.0} min: keepalive stopped",
                    max.as_secs_f64() / 60.0
                ));
                // Wait until this request completes or another request starts.
                let mut st = ka_state.lock().expect("keepalive lock");
                while st.active && !st.stop_all && st.req_gen == my_gen {
                    st = ka_cv.wait(st).expect("keepalive wait");
                }
                if st.stop_all {
                    return;
                }
                break;
            }

            // Lock ordering: always write_lock before ka_state so keepalive never writes after a reply.
            let write_ok = {
                let _w = write_lock.lock().expect("write lock");
                let still_active = {
                    let st = ka_state.lock().expect("keepalive lock");
                    st.active && !st.stop_all && st.req_gen == my_gen
                };
                if !still_active {
                    break;
                }
                pipe.write_frame(keepalive_header, &[]).is_ok()
            };
            if !write_ok {
                let mut st = ka_state.lock().expect("keepalive lock");
                while st.active && !st.stop_all && st.req_gen == my_gen {
                    st = ka_cv.wait(st).expect("keepalive wait");
                }
                if st.stop_all {
                    return;
                }
                break;
            }
            next_beat += every;
        }
    }
}

fn is_closed_io(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::BrokenPipe | io::ErrorKind::UnexpectedEof
    ) || matches!(e.raw_os_error(), Some(109 | 233 | 38))
}
