// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Live-reloading manager for the UI, public IPC, and MCP pipes (`PipeManager.cs`).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, Mutex};

use br_contracts::{AuditEntry, DataPaths, InstallLayout, IpcSettings, PipeNames, Settings};
use chrono::Utc;

use crate::cancel::ListenerGuard;
use crate::ipc::command_processor::CommandProcessor;
use crate::ipc::pipe_server::{PipeKind, PipeServer, RejectHandlerFn};
use crate::ipc::process_info::{PeerInfo, ProcessInfo};
use crate::log::Log;
use crate::settings_store::SettingsStore;
use crate::store::EncryptedStore;

pub type PipeNameFn = Arc<dyn Fn(PipeKind, &Settings) -> String + Send + Sync>;
pub type StoreGetterFn = Arc<dyn Fn() -> Option<Arc<EncryptedStore>> + Send + Sync>;
pub type ClientsChangedFn = Arc<dyn Fn(i32) + Send + Sync>;

struct ManagerState {
    ui: Option<Arc<PipeServer>>,
    public: Option<Arc<PipeServer>>,
    mcp: Option<Arc<PipeServer>>,
    public_ipc: Option<IpcSettings>,
    stopped: bool,
    settings_guard: Option<ListenerGuard>,
}

struct ManagerInner {
    settings: Arc<SettingsStore>,
    processor: Arc<CommandProcessor>,
    install_dir: PathBuf,
    store: StoreGetterFn,
    name_for: PipeNameFn,
    gate: Mutex<ManagerState>,
    ui_count: AtomicI32,
    public_count: AtomicI32,
    mcp_count: AtomicI32,
    clients_changed: Mutex<Vec<ClientsChangedFn>>,
}

/// Owns the UI pipe (always), the public pipe (`ipc.enabled`) and the MCP pipe (`mcp.enabled`),
/// and follows settings changes live.
pub struct PipeManager {
    inner: Arc<ManagerInner>,
}

impl PipeManager {
    pub fn new(
        settings: Arc<SettingsStore>,
        processor: Arc<CommandProcessor>,
        install_dir: &Path,
        store: StoreGetterFn,
        name_for: Option<PipeNameFn>,
    ) -> Self {
        Self {
            inner: Arc::new(ManagerInner {
                settings,
                processor,
                install_dir: install_dir.to_path_buf(),
                store,
                name_for: name_for.unwrap_or_else(|| Arc::new(Self::default_name)),
                gate: Mutex::new(ManagerState {
                    ui: None,
                    public: None,
                    mcp: None,
                    public_ipc: None,
                    stopped: false,
                    settings_guard: None,
                }),
                ui_count: AtomicI32::new(0),
                public_count: AtomicI32::new(0),
                mcp_count: AtomicI32::new(0),
                clients_changed: Mutex::new(Vec::new()),
            }),
        }
    }

    /// Default pipe names using `DataPaths::home()`.
    pub fn default_name(kind: PipeKind, s: &Settings) -> String {
        Self::default_name_for_home(kind, s, &DataPaths::home())
    }

    /// Default pipe names scoped to an explicit `home` (avoids mutating env vars in tests).
    pub fn default_name_for_home(kind: PipeKind, s: &Settings, home: &Path) -> String {
        let sid = PipeNames::user_sid().unwrap_or_default();
        match kind {
            PipeKind::Ui => PipeNames::ui(&sid, home),
            PipeKind::Mcp => PipeNames::mcp(&sid, home),
            PipeKind::Public => PipeNames::public(&s.ipc.pipe_name, &sid, home),
        }
    }

    pub fn ui(&self) -> Option<Arc<PipeServer>> {
        self.inner
            .gate
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .ui
            .clone()
    }

    pub fn public(&self) -> Option<Arc<PipeServer>> {
        self.inner
            .gate
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .public
            .clone()
    }

    pub fn mcp(&self) -> Option<Arc<PipeServer>> {
        self.inner
            .gate
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .mcp
            .clone()
    }

    pub fn on_clients_changed(&self, cb: impl Fn(i32) + Send + Sync + 'static) {
        self.inner
            .clients_changed
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(Arc::new(cb));
    }

    pub fn start(&self) {
        let weak = Arc::downgrade(&self.inner);
        let guard = self.inner.settings.on_changed_guarded(move |_s| {
            if let Some(inner) = weak.upgrade() {
                apply_settings(&inner);
            }
        });
        {
            let mut st = self.inner.gate.lock().unwrap_or_else(|e| e.into_inner());
            st.settings_guard = Some(guard);
        }
        apply_settings(&self.inner);
    }

    pub fn image_allowed(&self, kind: PipeKind, image: Option<&str>) -> bool {
        image_allowed_inner(&self.inner, kind, image)
    }

    /// B12 (D6): an `ipc.allowClients` entry inside any version folder of the agent's install root,
    /// or the stable `bin\betterrecall.cmd` shim, matches the same install-relative file in the
    /// agent's own install folder.
    pub fn same_install_file(install_dir: &Path, image: Option<&str>, entry: &str) -> bool {
        let Some(image) = image else {
            return false;
        };
        if entry.trim().is_empty() {
            return false;
        }
        let install = InstallLayout::real_path(install_dir)
            .to_string_lossy()
            .trim_end_matches('\\')
            .to_string();
        let img = InstallLayout::real_path(Path::new(image))
            .to_string_lossy()
            .into_owned();
        let ent = InstallLayout::real_path(Path::new(entry))
            .to_string_lossy()
            .into_owned();

        let install_prefix = format!("{install}\\");
        if img.len() < install_prefix.len()
            || !img.is_char_boundary(install_prefix.len())
            || !img[..install_prefix.len()].eq_ignore_ascii_case(&install_prefix)
        {
            return false;
        }
        let rel = &img[install_prefix.len()..];
        if Self::home_app_entry_matches(&ent, rel) {
            return true;
        }
        let Some(root) = Path::new(&install).parent() else {
            return false;
        };
        let root_str = root.to_string_lossy();
        let root_prefix = format!("{}\\", root_str.trim_end_matches('\\'));
        if ent.len() < root_prefix.len()
            || !ent.is_char_boundary(root_prefix.len())
            || !ent[..root_prefix.len()].eq_ignore_ascii_case(&root_prefix)
        {
            return false;
        }
        let in_root = &ent[root_prefix.len()..];
        let Some(cut) = in_root.find('\\') else {
            return false;
        };
        if cut == 0 {
            return false;
        }
        let folder = &in_root[..cut];
        let ent_rel = &in_root[cut + 1..];
        if folder.eq_ignore_ascii_case("bin") {
            return ent_rel.eq_ignore_ascii_case(InstallLayout::CLI_SHIM)
                && rel.eq_ignore_ascii_case(InstallLayout::CLI_EXE);
        }
        InstallLayout::is_version_folder(folder) && ent_rel.eq_ignore_ascii_case(rel)
    }

    /// B12 (0.3 layout): the install folder is fixed and the CLI shim lives in
    /// `<home>\app\bin\betterrecall.cmd`; 0.2.x entries named `<home>\app\<version>\<file>`.
    /// Either names the same file as `rel` (the client image relative to the install folder).
    fn home_app_entry_matches(ent: &str, rel: &str) -> bool {
        let parts: Vec<&str> = ent.split('\\').filter(|p| !p.is_empty()).collect();
        let rel_parts: Vec<&str> = rel.split('\\').filter(|p| !p.is_empty()).collect();
        if rel_parts.is_empty() {
            return false;
        }
        let shim = ["app", "bin", InstallLayout::CLI_SHIM];
        if parts.len() > shim.len()
            && parts[parts.len() - shim.len()..]
                .iter()
                .zip(shim)
                .all(|(a, b)| a.eq_ignore_ascii_case(b))
        {
            return rel.eq_ignore_ascii_case(InstallLayout::CLI_EXE);
        }
        let n = rel_parts.len() + 2;
        if parts.len() <= n {
            return false;
        }
        let at = parts.len() - n;
        parts[at].eq_ignore_ascii_case("app")
            && InstallLayout::is_version_folder(parts[at + 1])
            && parts[at + 2..]
                .iter()
                .zip(&rel_parts)
                .all(|(a, b)| a.eq_ignore_ascii_case(b))
    }

    /// True when a permission went from on to off (or `require_unlocked` became true).
    pub fn revoked(before: &IpcSettings, now: &IpcSettings) -> bool {
        (before.allow_control && !now.allow_control)
            || (before.allow_purge && !now.allow_purge)
            || (before.allow_thumbnails && !now.allow_thumbnails)
            || (!before.require_unlocked && now.require_unlocked)
    }

    pub fn stop(&self) {
        let servers = {
            let mut st = self.inner.gate.lock().unwrap_or_else(|e| e.into_inner());
            st.settings_guard = None;
            st.stopped = true;
            let mut v = Vec::new();
            if let Some(s) = st.ui.take() {
                v.push(s);
            }
            if let Some(s) = st.public.take() {
                v.push(s);
            }
            if let Some(s) = st.mcp.take() {
                v.push(s);
            }
            v
        };
        // C4 (sweep 5): stop all pipes at once; each waits up to 5 s for its handlers, and a
        // slow UI handler must not keep the public and MCP pipes serving past the session-end budget.
        std::thread::scope(|scope| {
            for s in &servers {
                scope.spawn(move || s.stop());
            }
        });
    }
}

impl Drop for PipeManager {
    fn drop(&mut self) {
        self.stop();
    }
}

fn image_allowed_inner(inner: &ManagerInner, kind: PipeKind, image: Option<&str>) -> bool {
    match kind {
        PipeKind::Ui => {
            let expected = InstallLayout::full(&inner.install_dir, InstallLayout::UI_EXE);
            ProcessInfo::same_path(image, Some(&expected.to_string_lossy()))
        }
        PipeKind::Mcp => {
            let expected = InstallLayout::full(&inner.install_dir, InstallLayout::MCP_EXE);
            ProcessInfo::same_path(image, Some(&expected.to_string_lossy()))
        }
        PipeKind::Public => {
            let allow = inner.settings.current().ipc.allow_clients;
            allow.is_empty()
                || allow.iter().any(|a| {
                    ProcessInfo::same_path(image, Some(a.as_str()))
                        || PipeManager::same_install_file(&inner.install_dir, image, a)
                })
        }
    }
}

fn try_create(inner: &Arc<ManagerInner>, kind: PipeKind, s: &Settings) -> Option<Arc<PipeServer>> {
    let name = (inner.name_for)(kind, s);
    let inner_allow = inner.clone();
    let proc = inner.processor.clone();
    let store_fn = inner.store.clone();
    let on_reject: RejectHandlerFn = Arc::new(move |k, peer: &PeerInfo, _why, count| {
        let label = match k {
            PipeKind::Ui => "ui",
            PipeKind::Mcp => "mcp",
            PipeKind::Public => "ipc",
        };
        if let Some(st) = store_fn() {
            let _ = st.append_audit(&AuditEntry {
                ts: Utc::now(),
                pipe: label.to_string(),
                client_pid: peer.pid,
                client_image: peer.image.clone().unwrap_or_default(),
                verb: "rejected".to_string(),
                result_count: count,
                query_sha256: String::new(),
            });
        }
    });
    let server = Arc::new(PipeServer::new(
        &name,
        kind,
        move |img| image_allowed_inner(&inner_allow, kind, img),
        move |conn, ct| proc.serve(conn, &ct),
        Some(on_reject),
    ));
    let inner_cnt = inner.clone();
    server.on_connected_changed(move |n| {
        match kind {
            PipeKind::Ui => inner_cnt.ui_count.store(n, Ordering::Release),
            PipeKind::Mcp => inner_cnt.mcp_count.store(n, Ordering::Release),
            PipeKind::Public => inner_cnt.public_count.store(n, Ordering::Release),
        }
        let total = inner_cnt.ui_count.load(Ordering::Acquire)
            + inner_cnt.public_count.load(Ordering::Acquire)
            + inner_cnt.mcp_count.load(Ordering::Acquire);
        let handlers = inner_cnt
            .clients_changed
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        for h in handlers {
            h(total);
        }
    });
    match server.start() {
        Ok(()) => Some(server),
        Err(e) => {
            Log::error_err(&format!("pipe {kind} could not be created"), &e.to_string());
            None
        }
    }
}

fn apply_settings(inner: &Arc<ManagerInner>) {
    let mut to_stop: Vec<Arc<PipeServer>> = Vec::new();
    {
        let mut st = inner.gate.lock().unwrap_or_else(|e| e.into_inner());
        if st.stopped {
            return;
        }
        // Always apply the newest settings snapshot (B13).
        let s = inner.settings.current();
        if st.ui.is_none() {
            st.ui = try_create(inner, PipeKind::Ui, &s);
        }

        if let (Some(pub_srv), Some(prev_ipc)) = (&st.public, &st.public_ipc) {
            let revoked = PipeManager::revoked(prev_ipc, &s.ipc);
            let dropped = pub_srv.drop_where(|peer| {
                revoked || !image_allowed_inner(inner, PipeKind::Public, peer.image.as_deref())
            });
            if dropped > 0 {
                Log::info(&format!(
                    "pipe Public: {dropped} connection(s) dropped after an ipc settings change"
                ));
            }
        }
        st.public_ipc = Some(IpcSettings {
            allow_control: s.ipc.allow_control,
            allow_purge: s.ipc.allow_purge,
            allow_thumbnails: s.ipc.allow_thumbnails,
            require_unlocked: s.ipc.require_unlocked,
            ..IpcSettings::default()
        });

        let want_public = if s.ipc.enabled {
            Some((inner.name_for)(PipeKind::Public, &s))
        } else {
            None
        };
        let want_base = want_public.as_deref().map(PipeNames::base);
        if let Some(ref pub_srv) = st.public
            && Some(pub_srv.name()) != want_base
            && let Some(old) = st.public.take() {
                to_stop.push(old);
            }
        if want_public.is_some() && st.public.is_none() {
            // Stop old public server first if renaming/recreating.
            for old in to_stop.drain(..) {
                old.stop();
            }
            st.public = try_create(inner, PipeKind::Public, &s);
        }

        if !s.mcp.enabled
            && let Some(old) = st.mcp.take()
        {
            to_stop.push(old);
        }
        if s.mcp.enabled && st.mcp.is_none() {
            st.mcp = try_create(inner, PipeKind::Mcp, &s);
        }
    }
    for s in to_stop {
        s.stop();
    }
}
