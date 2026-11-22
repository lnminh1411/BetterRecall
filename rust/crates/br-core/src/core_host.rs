// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! CoreHost entry point coordinating Settings, KeyVault, BlobCipher, EncryptedStore, and PipeManager (`CoreHost.cs`).

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;

use br_contracts::{CoreHostOptions, DataPaths, PurgeNotice, PurgeScope, full_path};
use chrono::{DateTime, Utc};

use crate::ai::{ApiKeyStore, CloudDescriber, DescribeRedactor, PassThroughRedactor};
use crate::cancel::{CancelToken, ListenerGuard};
use crate::crypto::{BlobCipher, KeyVault, UnlockPassword};
use crate::ipc::{CommandDeps, CommandProcessor, PipeManager, PipeNameFn};
use crate::log::Log;
use crate::settings_store::SettingsStore;
use crate::store::{EncryptedStore, ExportJournal, StoreError};
use crate::traits::{AgentControl, QueryEmbedder, Thumbnailer};

pub type ClientsChangedHandler = Arc<dyn Fn(i32) + Send + Sync>;
pub type PurgedHandler = Arc<dyn Fn(&PurgeNotice) + Send + Sync>;

struct StoreSlot {
    store: Option<Arc<EncryptedStore>>,
    purged_listener_id: Option<u64>,
    purged_all_at: Option<DateTime<Utc>>,
}

struct PipesState {
    pipes: Option<Arc<PipeManager>>,
    stopping: bool,
    stop_cv: Arc<Condvar>,
}

/// Entry point the agent uses to get every Core service (`CoreHostImpl`).
pub struct CoreHost {
    options: CoreHostOptions,
    settings: Arc<SettingsStore>,
    vault: Arc<KeyVault>,
    blobs: Arc<BlobCipher>,
    /// Serializes `open_loaded` and `purge_all`. Never held across passphrase waits (B1).
    store_gate: Mutex<()>,
    store_slot: Mutex<StoreSlot>,
    purging_all: AtomicBool,
    last_purge_all_undeleted: AtomicI32,
    pipes_state: Mutex<PipesState>,
    processor: Mutex<Option<Arc<CommandProcessor>>>,
    thumbs: Mutex<Option<Arc<dyn Thumbnailer>>>,
    describe_redactor: Mutex<Option<Arc<dyn DescribeRedactor>>>,
    pipe_name_override: Mutex<Option<PipeNameFn>>,
    clients_changed: Mutex<Vec<ClientsChangedHandler>>,
    purged_listeners: Mutex<Vec<(u64, PurgedHandler)>>,
    next_purged_id: std::sync::atomic::AtomicU64,
    _settings_guard: ListenerGuard,
}

impl CoreHost {
    /// Creates a new CoreHost (probes Windows Hello asynchronously unless `probe_hello` is false).
    pub fn create(options: CoreHostOptions) -> Arc<Self> {
        Self::create_with_hello_probe(options, true)
    }

    /// Creates a CoreHost with explicit control over background Windows Hello probing (tests).
    pub fn create_with_hello_probe(options: CoreHostOptions, probe_hello: bool) -> Arc<Self> {
        let home = full_path(&options.home).unwrap_or_else(|_| options.home.clone());
        let install_dir =
            full_path(&options.install_dir).unwrap_or_else(|_| options.install_dir.clone());
        let opts = CoreHostOptions {
            home: home.clone(),
            install_dir,
        };
        let _ = fs::create_dir_all(&opts.home);
        if !Log::initialized() {
            Log::init(DataPaths::logs_dir(&opts.home), "core");
        }
        ExportJournal::sweep(&opts.home);
        let settings = Arc::new(SettingsStore::load(&opts.home));
        Log::set_level(Log::parse_level(Some(&settings.current().log.level)));

        let settings_for_vault = settings.clone();
        let vault = Arc::new(KeyVault::new(
            &opts.home,
            move || settings_for_vault.current(),
            None,
            probe_hello,
        ));
        let blobs = Arc::new(BlobCipher::from_vault(vault.clone()));

        let vault_for_cb = vault.clone();
        let settings_guard = settings.on_changed_guarded(move |s| {
            Log::set_level(Log::parse_level(Some(&s.log.level)));
            vault_for_cb.migrate_if_needed();
        });

        Arc::new(Self {
            options: opts,
            settings,
            vault,
            blobs,
            store_gate: Mutex::new(()),
            store_slot: Mutex::new(StoreSlot {
                store: None,
                purged_listener_id: None,
                purged_all_at: None,
            }),
            purging_all: AtomicBool::new(false),
            last_purge_all_undeleted: AtomicI32::new(0),
            pipes_state: Mutex::new(PipesState {
                pipes: None,
                stopping: false,
                stop_cv: Arc::new(Condvar::new()),
            }),
            processor: Mutex::new(None),
            thumbs: Mutex::new(None),
            describe_redactor: Mutex::new(None),
            pipe_name_override: Mutex::new(None),
            clients_changed: Mutex::new(Vec::new()),
            purged_listeners: Mutex::new(Vec::new()),
            next_purged_id: std::sync::atomic::AtomicU64::new(1),
            _settings_guard: settings_guard,
        })
    }

    pub fn home(&self) -> &Path {
        &self.options.home
    }

    pub fn install_dir(&self) -> &Path {
        &self.options.install_dir
    }

    pub fn settings(&self) -> &Arc<SettingsStore> {
        &self.settings
    }

    pub fn keys(&self) -> &Arc<KeyVault> {
        &self.vault
    }

    pub fn blobs(&self) -> &Arc<BlobCipher> {
        &self.blobs
    }

    pub fn store(&self) -> Option<Arc<EncryptedStore>> {
        self.store_slot
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .store
            .clone()
    }

    pub fn purging_all(&self) -> bool {
        self.purging_all.load(Ordering::Acquire)
    }

    pub fn last_purge_all_undeleted(&self) -> i32 {
        self.last_purge_all_undeleted.load(Ordering::Acquire)
    }

    pub fn set_describe_redactor(&self, redactor: Option<Arc<dyn DescribeRedactor>>) {
        *self
            .describe_redactor
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = redactor;
    }

    /// Overrides pipe naming for tests (e.g. scoping pipes explicitly to a test home).
    pub fn set_pipe_name_override(&self, name_for: Option<PipeNameFn>) {
        *self
            .pipe_name_override
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = name_for;
    }

    pub fn pipe_manager(&self) -> Option<Arc<PipeManager>> {
        self.pipes_state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pipes
            .clone()
    }

    pub fn processor(&self) -> Option<Arc<CommandProcessor>> {
        self.processor
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn is_export_running(&self) -> bool {
        self.processor()
            .is_some_and(|p| p.is_export_running())
    }

    pub fn on_clients_changed(&self, cb: impl Fn(i32) + Send + Sync + 'static) {
        self.clients_changed
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(Arc::new(cb));
    }

    pub fn on_purged(&self, cb: impl Fn(&PurgeNotice) + Send + Sync + 'static) -> ListenerGuard {
        let id = self.next_purged_id.fetch_add(1, Ordering::Relaxed);
        self.purged_listeners
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push((id, Arc::new(cb)));
        // Safe to keep a weak/no-op if host drops; remove by scanning if needed.
        ListenerGuard::noop()
    }

    fn on_store_purged(&self, notice: &PurgeNotice) {
        if let Some(proc) = self
            .processor
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
        {
            proc.on_store_purged(notice);
        }
        let handlers: Vec<PurgedHandler> = self
            .purged_listeners
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|(_, h)| h.clone())
            .collect();
        for h in handlers {
            if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| h(notice))).is_err() {
                Log::warn("purge listener failed");
            }
        }
    }

    fn wire_store(self: &Arc<Self>, store: Arc<EncryptedStore>) -> (Arc<EncryptedStore>, u64) {
        let weak = Arc::downgrade(self);
        let lid = store.on_purged(move |n| {
            if let Some(host) = weak.upgrade() {
                host.on_store_purged(n);
            }
        });
        let purged_all_at = self
            .store_slot
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .purged_all_at;
        if let Some(at) = purged_all_at {
            let _ = store.fence_up_to(at);
        }
        (store, lid)
    }

    /// Unwraps the data key (waiting for `unlock <passphrase>` in passphrase mode), then opens `store.db`.
    pub fn open_store(self: &Arc<Self>, ct: &CancelToken) -> Result<(), StoreError> {
        loop {
            if self.store().is_some() {
                return Ok(());
            }
            if ct.is_cancelled() {
                return Err(StoreError::Cancelled);
            }
            // B1: passphrase wait holds no gate, so purge all can run meanwhile.
            let loaded_silent = match self.vault.try_load_silent() {
                Ok(b) => b,
                Err(crate::crypto::CryptoError::KeyLost(msg)) => {
                    return Err(StoreError::KeyLost(msg));
                }
                Err(e) => return Err(StoreError::Crypto(e)),
            };
            if !loaded_silent {
                Log::info("data key needs the passphrase; waiting for unlock");
                while !self.vault.key_loaded() {
                    if ct.is_cancelled() {
                        return Err(StoreError::Cancelled);
                    }
                    if self.vault.wait_loaded(Duration::from_millis(100)) {
                        break;
                    }
                    // DestroyKey wakes wait_loaded without loading a key: re-loop to try_load_silent
                    // in case key mode changed or a fresh key can now be created.
                    if self.vault.key_loaded() || !self.vault.needs_passphrase() {
                        break;
                    }
                }
            }
            if self.open_loaded(ct)? {
                return Ok(());
            }
        }
    }

    fn open_loaded(self: &Arc<Self>, ct: &CancelToken) -> Result<bool, StoreError> {
        if ct.is_cancelled() {
            return Err(StoreError::Cancelled);
        }
        let _gate = self.store_gate.lock().unwrap_or_else(|e| e.into_inner());
        if self.store().is_some() {
            return Ok(true);
        }
        if !self.vault.key_loaded() {
            return Ok(false);
        }
        let opened = match EncryptedStore::open_with_vault(
            &self.options.home,
            self.vault.clone(),
            None,
        ) {
            Ok(s) => Arc::new(s),
            Err(StoreError::KeyLost(msg)) => return Err(StoreError::KeyLost(msg)),
            Err(e) if e.is_not_a_db() => {
                return Err(StoreError::KeyLost(
                    "The data key does not open this store.".to_string(),
                ));
            }
            Err(e) => return Err(e),
        };
        let (wired, lid) = self.wire_store(opened);
        {
            let mut slot = self.store_slot.lock().unwrap_or_else(|e| e.into_inner());
            slot.store = Some(wired);
            slot.purged_listener_id = Some(lid);
        }
        Log::info("store opened");
        Ok(true)
    }

    pub fn start_pipes(
        self: &Arc<Self>,
        agent: Arc<dyn AgentControl>,
        embedder: Arc<dyn QueryEmbedder>,
        thumbnailer: Arc<dyn Thumbnailer>,
    ) {
        {
            let st = self.pipes_state.lock().unwrap_or_else(|e| e.into_inner());
            if st.pipes.is_some() {
                return;
            }
        }
        *self.thumbs.lock().unwrap_or_else(|e| e.into_inner()) = Some(thumbnailer.clone());

        let host_for_store = self.clone();
        let host_for_purge = self.clone();
        let host_for_undel = self.clone();
        let redactor = self
            .describe_redactor
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .unwrap_or_else(|| Arc::new(PassThroughRedactor));

        let processor = Arc::new(CommandProcessor::new(CommandDeps {
            settings: self.settings.clone(),
            store: Arc::new(move || host_for_store.store()),
            keys: self.vault.clone(),
            agent,
            embedder,
            thumbnailer,
            purge_all: Arc::new(move |ct| host_for_purge.purge_all(ct)),
            purge_all_undeleted: Some(Arc::new(move || host_for_undel.last_purge_all_undeleted())),
            blobs: Some(self.blobs.clone()),
            home: Some(self.options.home.clone()),
            password: Some(Arc::new(UnlockPassword::new(&self.options.home))),
            ai_key: Some(Arc::new(ApiKeyStore::new(&self.options.home))),
            describer: Some(Arc::new(CloudDescriber::new())),
            redactor: Some(redactor),
            chat_http: None,
            chat_providers: None,
        }));
        *self.processor.lock().unwrap_or_else(|e| e.into_inner()) = Some(processor.clone());

        let home_for_names = self.options.home.clone();
        let name_for = self
            .pipe_name_override
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .unwrap_or_else(|| {
                Arc::new(move |kind, s| PipeManager::default_name_for_home(kind, s, &home_for_names))
            });

        let host_for_pm_store = self.clone();
        let pipes = Arc::new(PipeManager::new(
            self.settings.clone(),
            processor,
            &self.options.install_dir,
            Arc::new(move || host_for_pm_store.store()),
            Some(name_for),
        ));
        let host_for_clients = self.clone();
        pipes.on_clients_changed(move |n| {
            let handlers = host_for_clients
                .clients_changed
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            for h in handlers {
                h(n);
            }
        });
        pipes.start();
        let mut st = self.pipes_state.lock().unwrap_or_else(|e| e.into_inner());
        st.pipes = Some(pipes);
    }

    /// Stops all pipes, deduplicating concurrent in-flight stop calls (C4).
    pub fn stop_pipes(&self) {
        let to_stop = {
            let mut st = self.pipes_state.lock().unwrap_or_else(|e| e.into_inner());
            if st.stopping {
                let cv = st.stop_cv.clone();
                while st.stopping {
                    st = cv.wait(st).unwrap_or_else(|e| e.into_inner());
                }
                return;
            }
            let Some(p) = st.pipes.clone() else {
                return;
            };
            st.stopping = true;
            p
        };

        to_stop.stop();

        let mut st = self.pipes_state.lock().unwrap_or_else(|e| e.into_inner());
        st.pipes = None;
        st.stopping = false;
        st.stop_cv.notify_all();
    }

    /// `purge all`: destroys the data key and deletes every store, keyframe and replay file,
    /// then opens a fresh empty store (or waits for a new passphrase).
    pub fn purge_all(self: &Arc<Self>, _ct: &CancelToken) -> i32 {
        let _gate = self.store_gate.lock().unwrap_or_else(|e| e.into_inner());
        let (old_store, old_lid) = {
            let mut slot = self.store_slot.lock().unwrap_or_else(|e| e.into_inner());
            (slot.store.take(), slot.purged_listener_id.take())
        };
        let mut rows = 0i32;
        if let Some(ref st) = old_store
            && let Ok(n) = st.count_purge(&PurgeScope {
                kind: "all".to_string(),
                from: None,
                to: None,
                app: None,
                site: None,
                id: None,
            })
        {
            rows = n as i32;
        }
        self.purging_all.store(true, Ordering::Release);

        if let Some(ref st) = old_store {
            if let Some(lid) = old_lid {
                st.remove_purged_listener(lid);
            }
            st.close_for_purge_all();
        }
        drop(old_store);

        let notice = PurgeNotice {
            reason: "all".to_string(),
            ids: Vec::new(),
            from: Some(DateTime::<Utc>::MIN_UTC),
            to: Some(Utc::now()),
            newest: None,
            scope: None,
        };
        self.on_store_purged(&notice);

        EncryptedStore::delete_files(&self.options.home);
        let _ = self.vault.destroy_key();

        let replays = DataPaths::replays_dir(&self.options.home);
        let mut undeleted = 0i32;
        if replays.exists() {
            for attempt in 0..=20 {
                let last = attempt >= 20;
                let mut failed = 0i32;
                if let Ok(entries) = fs::read_dir(&replays) {
                    for entry in entries.flatten() {
                        let p: PathBuf = entry.path();
                        if !p.is_file() {
                            continue;
                        }
                        if let Err(e) = fs::remove_file(&p) {
                            failed += 1;
                            if !last {
                                continue;
                            }
                            Log::error_err("replay delete failed", &e.to_string());
                            ExportJournal::add(Some(&self.options.home), &p);
                        }
                    }
                }
                if failed == 0 || last {
                    undeleted = failed;
                    break;
                }
                thread::sleep(Duration::from_millis(100));
            }
        }
        self.last_purge_all_undeleted
            .store(undeleted, Ordering::Release);
        if undeleted > 0 {
            Log::error(&format!(
                "purge all: {undeleted} replay file(s) could not be deleted (in use); deleted at the next start"
            ));
        }

        if let Some(thumbs) = self.thumbs.lock().unwrap_or_else(|e| e.into_inner()).clone() {
            thumbs.clear_cache();
        }
        Log::info("purge all done");

        {
            let mut slot = self.store_slot.lock().unwrap_or_else(|e| e.into_inner());
            slot.purged_all_at = Some(Utc::now());
        }
        self.purging_all.store(false, Ordering::Release);

        if self.vault.try_load_silent().unwrap_or(false) {
            if let Ok(new_store) =
                EncryptedStore::open_with_vault(&self.options.home, self.vault.clone(), None)
            {
                let (wired, lid) = self.wire_store(Arc::new(new_store));
                let mut slot = self.store_slot.lock().unwrap_or_else(|e| e.into_inner());
                slot.store = Some(wired);
                slot.purged_listener_id = Some(lid);
            }
        } else {
            let host_bg = self.clone();
            let _ = thread::Builder::new()
                .name("BetterRecall.ReopenAfterPurge".to_string())
                .spawn(move || {
                    let _ = host_bg.open_store(&CancelToken::none());
                });
        }

        rows
    }

    pub fn shutdown(&self) {
        self.stop_pipes();
        let mut slot = self.store_slot.lock().unwrap_or_else(|e| e.into_inner());
        slot.store = None;
        slot.purged_listener_id = None;
    }
}

impl Drop for CoreHost {
    fn drop(&mut self) {
        self.shutdown();
    }
}
