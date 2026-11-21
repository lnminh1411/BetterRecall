// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Overlapped Win32 named-pipe server with SDDL security and peer trust checks (`PipeServer.cs`).

use std::collections::HashMap;
use std::io;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use br_contracts::{Envelope, PipeNames};
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use windows::Win32::Foundation::{HANDLE, HLOCAL, LocalFree};
use windows::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
use windows::Win32::Storage::FileSystem::{
    FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED, PIPE_ACCESS_DUPLEX,
};
use windows::Win32::System::Pipes::{
    CreateNamedPipeW, GetNamedPipeClientProcessId, GetNamedPipeClientSessionId,
    PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_WAIT,
};
use windows::core::PCWSTR;

use crate::cancel::{CancelSource, CancelToken};
use crate::ipc::audit_throttle::AuditThrottle;
use crate::ipc::pipe_stream::PipeStream;
use crate::ipc::process_info::{PeerInfo, ProcessInfo};
use crate::log::Log;
use crate::win32::{self, SECURITY_MANDATORY_MEDIUM_RID, win32_to_io};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PipeKind {
    Ui,
    Public,
    Mcp,
}

impl std::fmt::Display for PipeKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Ui => write!(f, "Ui"),
            Self::Public => write!(f, "Public"),
            Self::Mcp => write!(f, "Mcp"),
        }
    }
}

/// A connected, trust-checked client.
pub struct PipeConnection {
    pub kind: PipeKind,
    pub peer: PeerInfo,
    pub stream: PipeStream,
}

pub type ImageAllowedFn = Arc<dyn Fn(Option<&str>) -> bool + Send + Sync>;
pub type ConnectionHandlerFn = Arc<dyn Fn(PipeConnection, CancelToken) + Send + Sync>;
pub type RejectHandlerFn = Arc<dyn Fn(PipeKind, &PeerInfo, &str, i32) + Send + Sync>;
pub type ConnectedChangedFn = Arc<dyn Fn(i32) + Send + Sync>;

struct ServerState {
    live: HashMap<usize, PipeStream>,
    admitted: HashMap<usize, (PipeStream, PeerInfo)>,
    loops: Vec<JoinHandle<()>>,
    connected_changed: Vec<ConnectedChangedFn>,
}

struct ServerInner {
    name: String,
    kind: PipeKind,
    image_allowed: ImageAllowedFn,
    handler: ConnectionHandlerFn,
    on_reject: Option<RejectHandlerFn>,
    rejects: AuditThrottle,
    cts: CancelSource,
    gate: Mutex<ServerState>,
    connected: AtomicI32,
}

/// Named-pipe server per research 7.3/7.4: SDDL with the user SID (deny NETWORK, medium mandatory
/// label), `PIPE_REJECT_REMOTE_CLIENTS`, `FILE_FLAG_FIRST_PIPE_INSTANCE` on the first instance,
/// 4 instances (8 for UI), byte mode, overlapped.
pub struct PipeServer {
    inner: Arc<ServerInner>,
}

impl PipeServer {
    pub const INSTANCES: usize = 4;
    pub const UI_INSTANCES: usize = 8;

    pub fn new(
        name: &str,
        kind: PipeKind,
        image_allowed: impl Fn(Option<&str>) -> bool + Send + Sync + 'static,
        handler: impl Fn(PipeConnection, CancelToken) + Send + Sync + 'static,
        on_reject: Option<RejectHandlerFn>,
    ) -> Self {
        Self {
            inner: Arc::new(ServerInner {
                name: PipeNames::base(name).to_string(),
                kind,
                image_allowed: Arc::new(image_allowed),
                handler: Arc::new(handler),
                on_reject,
                rejects: AuditThrottle::new(ChronoDuration::minutes(1), 10, 512),
                cts: CancelSource::new(),
                gate: Mutex::new(ServerState {
                    live: HashMap::new(),
                    admitted: HashMap::new(),
                    loops: Vec::new(),
                    connected_changed: Vec::new(),
                }),
                connected: AtomicI32::new(0),
            }),
        }
    }

    pub fn name(&self) -> &str {
        &self.inner.name
    }

    pub fn kind(&self) -> PipeKind {
        self.inner.kind
    }

    pub fn max_instances(&self) -> usize {
        if self.inner.kind == PipeKind::Ui {
            Self::UI_INSTANCES
        } else {
            Self::INSTANCES
        }
    }

    pub fn connected(&self) -> i32 {
        self.inner.connected.load(Ordering::Acquire)
    }

    pub fn on_connected_changed(&self, cb: impl Fn(i32) + Send + Sync + 'static) {
        self.inner
            .gate
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .connected_changed
            .push(Arc::new(cb));
    }

    /// SDDL from research 7.3.
    pub fn sddl(sid: &str) -> String {
        format!("O:{sid}D:P(D;;GA;;;NU)(A;;GRGW;;;{sid})S:(ML;;NWNR;;;ME)")
    }

    /// Reads back the security descriptor of a pipe handle as SDDL (owner, DACL, label).
    ///
    /// # Safety
    /// `handle` must be a valid Win32 pipe handle.
    pub unsafe fn read_sddl(handle: HANDLE) -> Option<String> {
        unsafe { win32::read_kernel_sddl(handle) }
    }

    /// Creates a single overlapped named-pipe instance with the SDDL security descriptor.
    pub fn create_instance(&self, first: bool) -> io::Result<PipeStream> {
        create_pipe_instance(&self.inner.name, self.max_instances() as u32, first)
    }

    /// Creates the first instance (`FILE_FLAG_FIRST_PIPE_INSTANCE`) and starts the accept threads.
    /// Fails when the name is already taken.
    pub fn start(&self) -> io::Result<()> {
        let first = self.create_instance(true)?;
        let count = self.max_instances();
        let mut st = self.inner.gate.lock().unwrap_or_else(|e| e.into_inner());
        for i in 0..count {
            let inner = self.inner.clone();
            let initial = if i == 0 { Some(first.clone()) } else { None };
            let h = thread::Builder::new()
                .name(format!("BetterRecall.Pipe.{}.{}", inner.kind, i))
                .spawn(move || accept_loop(inner, initial))?;
            st.loops.push(h);
        }
        Log::info(&format!("pipe {} listening", self.inner.kind));
        Ok(())
    }

    /// Returns `None` when the peer passes research 7.4 server checks, else a short reason.
    pub fn check(&self, p: &PeerInfo, connected_utc: Option<DateTime<Utc>>) -> Option<&'static str> {
        check_peer(&self.inner.image_allowed, p, connected_utc)
    }

    /// Closes every served connection whose peer matches `drop`. Synchronous.
    pub fn drop_where(&self, drop_pred: impl Fn(&PeerInfo) -> bool) -> usize {
        let victims: Vec<PipeStream> = {
            let st = self.inner.gate.lock().unwrap_or_else(|e| e.into_inner());
            st.admitted
                .values()
                .filter(|(_, peer)| drop_pred(peer))
                .map(|(s, _)| s.clone())
                .collect()
        };
        for s in &victims {
            s.close();
        }
        victims.len()
    }

    /// Stops accepting and closes all live connections, waiting up to 5 s for accept threads.
    pub fn stop(&self) {
        self.inner.cts.cancel();
        let live: Vec<PipeStream> = {
            let st = self.inner.gate.lock().unwrap_or_else(|e| e.into_inner());
            st.live.values().cloned().collect()
        };
        for s in live {
            s.close();
        }
        let loops: Vec<JoinHandle<()>> = {
            let mut st = self.inner.gate.lock().unwrap_or_else(|e| e.into_inner());
            std::mem::take(&mut st.loops)
        };
        let deadline = Instant::now() + Duration::from_secs(5);
        for handle in loops {
            while !handle.is_finished() && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(10));
            }
            if handle.is_finished() {
                let _ = handle.join();
            }
        }
        Log::info(&format!("pipe {} stopped", self.inner.kind));
    }
}

impl Drop for PipeServer {
    fn drop(&mut self) {
        self.stop();
    }
}

fn create_pipe_instance(name: &str, max_instances: u32, first: bool) -> io::Result<PipeStream> {
    let sid = ProcessInfo::current_sid();
    let sddl = PipeServer::sddl(sid);
    let sddl_w: Vec<u16> = sddl.encode_utf16().chain(Some(0)).collect();
    let mut sd = PSECURITY_DESCRIPTOR::default();
    unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            PCWSTR(sddl_w.as_ptr()),
            SDDL_REVISION_1,
            &mut sd,
            None,
        )
    }
    .map_err(win32_to_io)?;

    struct SdGuard(PSECURITY_DESCRIPTOR);
    impl Drop for SdGuard {
        fn drop(&mut self) {
            if !self.0.0.is_null() {
                unsafe {
                    let _ = LocalFree(Some(HLOCAL(self.0.0)));
                }
            }
        }
    }
    let _sd_guard = SdGuard(sd);

    let sa = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: sd.0,
        bInheritHandle: false.into(),
    };
    let open = PIPE_ACCESS_DUPLEX
        | FILE_FLAG_OVERLAPPED
        | if first {
            FILE_FLAG_FIRST_PIPE_INSTANCE
        } else {
            windows::Win32::Storage::FileSystem::FILE_FLAGS_AND_ATTRIBUTES(0)
        };
    let mode = PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS;
    let full_name = format!(r"\\.\pipe\{name}");
    let name_w: Vec<u16> = full_name.encode_utf16().chain(Some(0)).collect();

    let h = unsafe {
        CreateNamedPipeW(
            PCWSTR(name_w.as_ptr()),
            open,
            mode,
            max_instances,
            65_536,
            65_536,
            0,
            Some(&sa),
        )
    };
    if h.is_invalid() {
        let err = io::Error::last_os_error();
        return Err(io::Error::new(
            err.kind(),
            format!(
                "CreateNamedPipe failed ({}).",
                err.raw_os_error().unwrap_or(0)
            ),
        ));
    }
    unsafe { PipeStream::from_raw_handle(h) }
}

fn accept_loop(inner: Arc<ServerInner>, mut next: Option<PipeStream>) {
    let ct = inner.cts.token();
    let max_instances = if inner.kind == PipeKind::Ui {
        PipeServer::UI_INSTANCES as u32
    } else {
        PipeServer::INSTANCES as u32
    };
    while !ct.is_cancelled() {
        let stream = match next.take() {
            Some(s) => s,
            None => match create_pipe_instance(&inner.name, max_instances, false) {
                Ok(s) => s,
                Err(e) => {
                    if ct.is_cancelled() {
                        break;
                    }
                    Log::warn_err(
                        &format!("pipe {}: instance create failed", inner.kind),
                        &e.to_string(),
                    );
                    if ct.wait_timeout(Duration::from_secs(1)) {
                        break;
                    }
                    continue;
                }
            },
        };
        let sid = stream.id();
        {
            let mut st = inner.gate.lock().unwrap_or_else(|e| e.into_inner());
            st.live.insert(sid, stream.clone());
        }
        let connect_res = stream.wait_for_connection(&ct);
        if connect_res.is_ok() {
            let connected_utc = ProcessInfo::start_bound(Utc::now());
            serve_connection(&inner, stream.clone(), connected_utc, &ct);
        } else if !ct.is_cancelled() && !stream.is_closed()
            && let Err(e) = connect_res {
                Log::warn_err(
                    &format!("pipe {}: connection ended with error", inner.kind),
                    &e.to_string(),
                );
            }
        {
            let mut st = inner.gate.lock().unwrap_or_else(|e| e.into_inner());
            st.live.remove(&sid);
        }
        // Drop stream -> CloseHandle without DisconnectNamedPipe.
        drop(stream);
    }
}

fn serve_connection(
    inner: &ServerInner,
    stream: PipeStream,
    connected_utc: DateTime<Utc>,
    ct: &CancelToken,
) {
    let mut pid = 0u32;
    let mut session = 0u32;
    unsafe {
        let _ = GetNamedPipeClientProcessId(stream.raw_handle(), &mut pid);
        let _ = GetNamedPipeClientSessionId(stream.raw_handle(), &mut session);
    }
    let peer = ProcessInfo::read(pid as i32, session);
    if let Some(why) = check_peer(&inner.image_allowed, &peer, Some(connected_utc)) {
        let key = format!("{why}|{}", peer.image.as_deref().unwrap_or(""));
        let times = inner.rejects.admit(&key, Utc::now());
        if times > 0 {
            let suffix = if times > 1 {
                format!(", {times} times since the last entry")
            } else {
                String::new()
            };
            Log::warn(&format!(
                "pipe {}: client pid {pid} rejected ({why}){suffix}",
                inner.kind
            ));
            if let Some(ref on_reject) = inner.on_reject {
                on_reject(inner.kind, &peer, why, times);
            }
        }
        let _ = stream.write_line(
            &Envelope::error("Client not allowed."),
            Some(Duration::from_secs(2)),
            ct,
        );
        return;
    }

    let n = inner.connected.fetch_add(1, Ordering::AcqRel) + 1;
    raise_connected(inner, n);
    let sid = stream.id();
    {
        let mut st = inner.gate.lock().unwrap_or_else(|e| e.into_inner());
        st.admitted.insert(sid, (stream.clone(), peer.clone()));
    }
    (inner.handler)(
        PipeConnection {
            kind: inner.kind,
            peer,
            stream,
        },
        ct.clone(),
    );
    {
        let mut st = inner.gate.lock().unwrap_or_else(|e| e.into_inner());
        st.admitted.remove(&sid);
    }
    let n = inner.connected.fetch_sub(1, Ordering::AcqRel) - 1;
    raise_connected(inner, n);
}

fn raise_connected(inner: &ServerInner, n: i32) {
    let handlers = {
        let st = inner.gate.lock().unwrap_or_else(|e| e.into_inner());
        st.connected_changed.clone()
    };
    for h in handlers {
        h(n);
    }
}

fn check_peer(
    image_allowed: &ImageAllowedFn,
    p: &PeerInfo,
    connected_utc: Option<DateTime<Utc>>,
) -> Option<&'static str> {
    if let Some(at) = connected_utc {
        match p.created_utc {
            Some(created) if created <= at => {}
            _ => return Some("pid reuse"),
        }
    }
    if p.session != ProcessInfo::current_session() {
        return Some("session");
    }
    match p.sid.as_deref() {
        Some(sid) if sid.eq_ignore_ascii_case(ProcessInfo::current_sid()) => {}
        _ => return Some("user"),
    }
    if p.integrity_rid < SECURITY_MANDATORY_MEDIUM_RID {
        return Some("integrity");
    }
    if p.app_container {
        return Some("appcontainer");
    }
    if !image_allowed(p.image.as_deref()) {
        return Some("image");
    }
    None
}
