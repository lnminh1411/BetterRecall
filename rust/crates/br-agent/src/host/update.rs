// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Signed auto-update against GitHub Releases (R8, stage I5).

use std::cell::RefCell;
use std::fmt::Write as _;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use br_contracts::{DataPaths, UpdateProgress, UpdateStatus};
use br_core::{AgentError, CancelToken, CommandProcessor, win32};
use br_fetch::{Downloader, FetchError, FetchProgress};
use br_integrity::{UPDATE_JSON, UPDATE_SIG, UpdateManifest, verify_update_manifest};
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use semver::Version;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use windows::Win32::Foundation::{CloseHandle, ERROR_ACCESS_DENIED, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Storage::FileSystem::FILE_SHARE_READ;
use windows::Win32::System::Threading::{
    CREATE_BREAKAWAY_FROM_JOB, CREATE_NEW_PROCESS_GROUP, CreateProcessW, DETACHED_PROCESS,
    GetExitCodeProcess, INFINITE, PROCESS_CREATION_FLAGS, PROCESS_INFORMATION, STARTUPINFOW,
    WaitForSingleObject,
};
use windows::core::{PCWSTR, PWSTR};

use crate::util::AgentLog;

pub const PROD_FEED_URL: &str =
    "https://api.github.com/repos/lnminh1411/BetterRecall/releases?per_page=20";
pub const PROD_ALLOWED_PREFIX: &str = "https://github.com/lnminh1411/BetterRecall/releases/";

pub const MAX_FEED_BYTES: usize = 2 * 1024 * 1024;
pub const MAX_MANIFEST_BYTES: usize = 64 * 1024;
pub const MAX_SIG_BYTES: usize = 64 * 1024;
pub const MAX_INSTALLER_BYTES: u64 = 4 * 1024 * 1024 * 1024;

pub const NO_KEY_REASON: &str =
    "This build cannot verify updates (it was built without the release key).";
pub const RATE_LIMIT_ERROR: &str = "GitHub refused the request (rate limit). Try again later.";
pub const OFFLINE_ERROR: &str = "No internet connection.";

pub const AUTO_INITIAL_DELAY: Duration = Duration::from_secs(10 * 60);
pub const AUTO_WAKE_INTERVAL: Duration = Duration::from_secs(15 * 60);
pub const AUTO_CHECK_AGE_HOURS: i64 = 24;
pub const AUTO_IDLE_THRESHOLD_MS: i64 = 10 * 60 * 1000;

const PROGRESS_MIN_INTERVAL: Duration = Duration::from_millis(250);
const HASH_BUF_LEN: usize = 1024 * 1024;
const STATE_FILE_NAME: &str = "state.json";

/// Test launcher hook invoked instead of `CreateProcessW` so tests never start a process.
pub type InstallerLauncher =
    Arc<dyn Fn(&Path, &[String]) -> io::Result<Option<HANDLE>> + Send + Sync>;

// C1: RAII wrapper for installer process handle
struct ProcessHandle(HANDLE);
unsafe impl Send for ProcessHandle {}
impl Drop for ProcessHandle {
    fn drop(&mut self) {
        if !self.0.is_invalid() && !self.0.0.is_null() {
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }
}

/// Test-only seam configuring feed URL, allowed prefix, verifying key, version, retries, and launcher.
/// Production never reads any of these from settings, environment, or files.
#[derive(Clone)]
pub struct UpdateTestConfig {
    pub feed_url: String,
    pub allowed_prefix: String,
    pub public_key_pem: Option<String>,
    pub current_version: Option<String>,
    pub fast_retries: bool,
    pub launcher: Option<InstallerLauncher>,
}

/// Outcome of one scheduler evaluation step (`UpdateManager::auto_step`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutoStepOutcome {
    Disabled,
    NoUpdate,
    Downloaded,
    Deferred,
    Applied,
    Failed,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PersistedState {
    #[serde(default, with = "br_contracts::utc_timestamp::opt")]
    last_check: Option<DateTime<Utc>>,
    #[serde(default)]
    latest: Option<PersistedLatest>,
    #[serde(default)]
    downloaded: Option<String>,
    // C1: Backoff and failure tracking
    #[serde(default)]
    failed_version: Option<String>,
    #[serde(default)]
    failed_count: u32,
    #[serde(default, with = "br_contracts::utc_timestamp::opt")]
    failed_at: Option<DateTime<Utc>>,
    #[serde(default)]
    last_install_handled_utc: Option<String>,
}

impl PersistedState {
    fn record_failure(&mut self, version: &str, now: DateTime<Utc>) {
        if self.failed_version.as_deref() == Some(version) {
            self.failed_count = self.failed_count.saturating_add(1);
        } else {
            self.failed_version = Some(version.to_string());
            self.failed_count = 1;
        }
        self.failed_at = Some(now);
    }
}

// C1: Installer report written by installer slice to last-install.json
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LastInstallReport {
    version: String,
    exit_code: i32,
    utc: String,
}

// C1: Backoff: 1 h doubling per failure, capped at 24 h
fn failure_backoff(failed_count: u32) -> ChronoDuration {
    if failed_count == 0 {
        return ChronoDuration::zero();
    }
    let shift = (failed_count - 1).min(30);
    let hours = (1i64 << shift).min(24);
    ChronoDuration::hours(hours)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PersistedLatest {
    version: String,
    #[serde(default)]
    notes_url: Option<String>,
    #[serde(default, with = "br_contracts::utc_timestamp::opt")]
    published_at: Option<DateTime<Utc>>,
    #[serde(default)]
    installer_bytes: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct GhRelease {
    #[serde(default)]
    tag_name: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    html_url: Option<String>,
    #[serde(default)]
    published_at: Option<String>,
    #[serde(default)]
    assets: Vec<GhAsset>,
}

#[derive(Debug, Deserialize)]
struct GhAsset {
    #[serde(default)]
    name: String,
    #[serde(default)]
    size: u64,
    #[serde(default)]
    browser_download_url: String,
}

struct VerifiedRelease {
    version: Version,
    notes_url: Option<String>,
    published_at: Option<DateTime<Utc>>,
    manifest: UpdateManifest,
    manifest_bytes: Vec<u8>,
    sig_bytes: Vec<u8>,
    installer_url: String,
}

enum CheckOutcome {
    NoRelease,
    Unverifiable {
        version: Version,
        notes_url: Option<String>,
        published_at: Option<DateTime<Utc>>,
    },
    Verified(Box<VerifiedRelease>),
    Rejected(String),
    Offline,
    Cancelled,
}

struct EffectiveConfig {
    feed_url: String,
    allowed_prefix: String,
    public_key_pem: Option<String>,
    current_version: String,
    fast_retries: bool,
    launcher: Option<InstallerLauncher>,
}

struct RuntimeState {
    state: String,
    error: Option<String>,
    // D3: True when `state == "error"` came from an install failure (not a feed/download error).
    install_error: bool,
    persisted: PersistedState,
}

/// Manages release checking, verified downloading, applying, and auto-update scheduling.
pub struct UpdateManager {
    home: PathBuf,
    test_config: Mutex<Option<UpdateTestConfig>>,
    runtime: Arc<Mutex<RuntimeState>>,
    update_downloading: Arc<AtomicBool>,
    models_downloading: Arc<AtomicBool>,
}

impl UpdateManager {
    pub fn new(
        home: PathBuf,
        update_downloading: Arc<AtomicBool>,
        models_downloading: Arc<AtomicBool>,
    ) -> Self {
        Self::new_with_version(
            home,
            update_downloading,
            models_downloading,
            env!("CARGO_PKG_VERSION"),
        )
    }

    #[doc(hidden)]
    pub fn new_with_version(
        home: PathBuf,
        update_downloading: Arc<AtomicBool>,
        models_downloading: Arc<AtomicBool>,
        running_version: &str,
    ) -> Self {
        let mut persisted = load_state(&updates_state_path(&home));
        let mut initial_state = if persisted.downloaded.is_some() {
            "ready".to_string()
        } else {
            "idle".to_string()
        };
        let mut initial_error = None;
        let mut initial_install_error = false;

        // C1: Read last-install.json at start
        let last_install_path = DataPaths::updates_dir(&home).join("last-install.json");
        if let Ok(content) = fs::read_to_string(&last_install_path)
            && let Ok(report) = serde_json::from_str::<LastInstallReport>(&content)
        {
            // C3: Equality check (not time ordering) so a report written after a backward clock
            // step is still handled once.
            let is_new = persisted.last_install_handled_utc.as_deref() != Some(&report.utc);

            if is_new {
                persisted.last_install_handled_utc = Some(report.utc.clone());
                if report.exit_code != 0 {
                    // C4: A non-zero exit whose version equals the running version means the swap
                    // succeeded and only a post-swap step failed; warn without recording an error.
                    let matches_running = report.version == running_version
                        || matches!(
                            (Version::parse(&report.version), Version::parse(running_version)),
                            (Ok(a), Ok(b)) if a == b
                        );
                    if matches_running {
                        AgentLog::warn(&format!(
                            "update post-swap step failed for {} (setup exit code {}); running version already updated",
                            report.version, report.exit_code
                        ));
                    } else if persisted.downloaded.as_deref() == Some(&report.version) {
                        initial_state = "error".to_string();
                        let err_msg = format!(
                            "Update could not be installed (setup exit code {}).",
                            report.exit_code
                        );
                        initial_error = Some(err_msg);
                        initial_install_error = true;
                        let report_time = DateTime::parse_from_rfc3339(&report.utc)
                            .map(|dt| dt.with_timezone(&Utc))
                            .unwrap_or_else(|_| Utc::now());
                        persisted.record_failure(&report.version, report_time);
                    }
                }
                let _ = save_state(&updates_state_path(&home), &persisted);
            }
        }

        Self {
            home,
            test_config: Mutex::new(None),
            runtime: Arc::new(Mutex::new(RuntimeState {
                state: initial_state,
                error: initial_error,
                install_error: initial_install_error,
                persisted,
            })),
            update_downloading,
            models_downloading,
        }
    }

    pub fn set_test_config(&self, cfg: Option<UpdateTestConfig>) {
        *self.test_config.lock().unwrap_or_else(|e| e.into_inner()) = cfg;
    }

    pub fn has_public_key(&self) -> bool {
        self.effective_config().public_key_pem.is_some()
    }

    /// Removes any downloaded update folder whose version is not strictly newer than the running version.
    pub fn cleanup_on_start(&self) {
        let cfg = self.effective_config();
        let Ok(running) = Version::parse(&cfg.current_version) else {
            return;
        };
        let mut rt = self.runtime.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(dl_ver_str) = rt.persisted.downloaded.clone() {
            let is_newer = Version::parse(&dl_ver_str).is_ok_and(|v| v > running);
            if !is_newer {
                let folder = DataPaths::updates_dir(&self.home).join(&dl_ver_str);
                if folder.exists() {
                    let _ = fs::remove_dir_all(&folder);
                }
                AgentLog::info(&format!(
                    "update cleanup: removed stale folder for version {dl_ver_str} (running {})",
                    cfg.current_version
                ));
                rt.persisted.downloaded = None;
                if rt.state == "ready" {
                    rt.state = "idle".to_string();
                }
                let _ = save_state(&updates_state_path(&self.home), &rt.persisted);
            }
        }

        // C1/C4: A successful install of a newer version (running version >= failedVersion) clears them.
        if let Some(ref fv_str) = rt.persisted.failed_version
            && Version::parse(fv_str).is_ok_and(|fv| running >= fv)
        {
            AgentLog::info(&format!(
                "update: clearing failure state for {fv_str} (running {})",
                cfg.current_version
            ));
            rt.persisted.failed_version = None;
            rt.persisted.failed_count = 0;
            rt.persisted.failed_at = None;
            // C4: Clear install-failure error state once the running version is at or past the failed version.
            if rt.state == "error" && rt.install_error {
                rt.state = if rt.persisted.downloaded.is_some()
                    && rt
                        .persisted
                        .latest
                        .as_ref()
                        .is_none_or(|l| Some(l.version.as_str()) == rt.persisted.downloaded.as_deref())
                {
                    "ready".to_string()
                } else {
                    "idle".to_string()
                };
                rt.error = None;
                rt.install_error = false;
            }
            let _ = save_state(&updates_state_path(&self.home), &rt.persisted);
        }
    }

    /// Returns current update status from memory and `state.json` without any network call.
    pub fn status(&self, auto_enabled: bool) -> UpdateStatus {
        let cfg = self.effective_config();
        let rt = self.runtime.lock().unwrap_or_else(|e| e.into_inner());
        self.build_status(&cfg, &rt, auto_enabled)
    }

    /// Queries the release feed, verifies signature/manifest when a key is present, updates `state.json`,
    /// and returns the resulting [`UpdateStatus`].
    pub fn check(&self, auto_enabled: bool, ct: &CancelToken) -> Result<UpdateStatus, AgentError> {
        self.check_at(Utc::now(), auto_enabled, ct)
    }

    pub fn check_at(
        &self,
        now: DateTime<Utc>,
        auto_enabled: bool,
        ct: &CancelToken,
    ) -> Result<UpdateStatus, AgentError> {
        if ct.is_cancelled() {
            return Err(AgentError::Cancelled);
        }
        let cfg = self.effective_config();
        let outcome = self.run_feed_check(&cfg, ct);

        let mut rt = self.runtime.lock().unwrap_or_else(|e| e.into_inner());
        if matches!(outcome, CheckOutcome::Cancelled) {
            // C4: A cancelled check logs one line and leaves state as it was before the check.
            AgentLog::info("update check cancelled");
            return Err(AgentError::Cancelled);
        }

        if let Ok(running) = Version::parse(&cfg.current_version)
            && let Some(ref fv_str) = rt.persisted.failed_version
            && Version::parse(fv_str).is_ok_and(|fv| running >= fv)
        {
            rt.persisted.failed_version = None;
            rt.persisted.failed_count = 0;
            rt.persisted.failed_at = None;
            let _ = save_state(&updates_state_path(&self.home), &rt.persisted);
        }

        let is_applying = rt.state == "applying";
        let keep_install_error = |rt: &RuntimeState, latest: Option<&str>| {
            rt.state == "error"
                && rt.install_error
                && latest.is_some()
                && rt.persisted.failed_version.as_deref() == latest
        };

        match outcome {
            CheckOutcome::Cancelled => unreachable!(),
            CheckOutcome::NoRelease => {
                AgentLog::info(&format!(
                    "update check: no release found (current={})",
                    cfg.current_version
                ));
                rt.persisted.last_check = Some(now);
                rt.persisted.latest = None;
                // D3: Do not overwrite `applying` state.
                if !is_applying {
                    rt.state = if rt.persisted.downloaded.is_some() {
                        "ready".to_string()
                    } else {
                        "idle".to_string()
                    };
                    rt.error = None;
                    rt.install_error = false;
                }
                let _ = save_state(&updates_state_path(&self.home), &rt.persisted);
            }
            CheckOutcome::Unverifiable {
                version,
                notes_url,
                published_at,
            } => {
                let ver_str = version.to_string();
                AgentLog::info(&format!(
                    "update check: latest={ver_str} current={} (no release key)",
                    cfg.current_version
                ));
                rt.persisted.last_check = Some(now);
                rt.persisted.latest = Some(PersistedLatest {
                    version: ver_str.clone(),
                    notes_url,
                    published_at,
                    installer_bytes: None,
                });
                // D3: Keep `applying` and keep an install-failure error while latest is still the failed version.
                if !is_applying && !keep_install_error(&rt, Some(&ver_str)) {
                    rt.state = "idle".to_string();
                    rt.error = None;
                    rt.install_error = false;
                }
                let _ = save_state(&updates_state_path(&self.home), &rt.persisted);
            }
            CheckOutcome::Verified(rel) => {
                let ver_str = rel.version.to_string();
                AgentLog::info(&format!(
                    "update check: verified latest={ver_str} current={}",
                    cfg.current_version
                ));
                rt.persisted.last_check = Some(now);
                rt.persisted.latest = Some(PersistedLatest {
                    version: ver_str.clone(),
                    notes_url: rel.notes_url,
                    published_at: rel.published_at,
                    installer_bytes: Some(rel.manifest.size),
                });
                // D3: Keep `applying` and keep an install-failure error while latest is still the failed version.
                if !is_applying && !keep_install_error(&rt, Some(&ver_str)) {
                    rt.state = if rt.persisted.downloaded.as_deref() == Some(ver_str.as_str()) {
                        "ready".to_string()
                    } else {
                        "idle".to_string()
                    };
                    rt.error = None;
                    rt.install_error = false;
                }
                let _ = save_state(&updates_state_path(&self.home), &rt.persisted);
            }
            CheckOutcome::Rejected(reason) => {
                AgentLog::warn(&format!("update check rejected: {reason}"));
                if !is_applying {
                    rt.state = "error".to_string();
                    rt.error = Some(reason);
                    rt.install_error = false;
                }
            }
            CheckOutcome::Offline => {
                AgentLog::warn("update check failed: offline");
                if !is_applying {
                    rt.state = "error".to_string();
                    rt.error = Some(OFFLINE_ERROR.to_string());
                    rt.install_error = false;
                }
            }
        }
        Ok(self.build_status(&cfg, &rt, auto_enabled))
    }

    /// Re-checks the feed, downloads and verifies the installer into `<home>\cache\updates\<version>\`,
    /// streaming throttled [`UpdateProgress`] updates.
    pub fn download(
        &self,
        emit: &mut dyn FnMut(UpdateProgress),
        ct: &CancelToken,
    ) -> Result<(), AgentError> {
        self.download_at(Utc::now(), emit, ct)
    }

    pub fn download_at(
        &self,
        now: DateTime<Utc>,
        emit: &mut dyn FnMut(UpdateProgress),
        ct: &CancelToken,
    ) -> Result<(), AgentError> {
        let cfg = self.effective_config();
        if cfg.public_key_pem.is_none() {
            return Err(AgentError::InvalidOperation(NO_KEY_REASON.to_string()));
        }
        if self.models_downloading.load(Ordering::Acquire) {
            return Err(AgentError::InvalidOperation(
                CommandProcessor::ERR_MODELS_RUNNING_FOR_UPDATE.to_string(),
            ));
        }
        if self
            .update_downloading
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(AgentError::InvalidOperation(
                CommandProcessor::ERR_UPDATE_BUSY.to_string(),
            ));
        }
        let _flag_guard = FlagGuard {
            flag: &self.update_downloading,
        };
        if self.models_downloading.load(Ordering::Acquire) {
            return Err(AgentError::InvalidOperation(
                CommandProcessor::ERR_MODELS_RUNNING_FOR_UPDATE.to_string(),
            ));
        }

        self.set_state("checking", None);
        let sink = UpdateProgressSink::new(emit);
        sink.emit_now(UpdateProgress {
            phase: "checking".to_string(),
            done_bytes: 0,
            total_bytes: 0,
            error: None,
        });

        if ct.is_cancelled() {
            self.set_state("idle", None);
            sink.emit_now(UpdateProgress {
                phase: "cancelled".to_string(),
                done_bytes: 0,
                total_bytes: 0,
                error: None,
            });
            return Ok(());
        }

        let running = Version::parse(&cfg.current_version).map_err(|e| {
            AgentError::Other(format!("invalid running version '{}': {e}", cfg.current_version))
        })?;

        let rel = match self.run_feed_check(&cfg, ct) {
            CheckOutcome::Cancelled => {
                AgentLog::info("update download cancelled during check");
                self.set_state("idle", None);
                sink.emit_now(UpdateProgress {
                    phase: "cancelled".to_string(),
                    done_bytes: 0,
                    total_bytes: 0,
                    error: None,
                });
                return Ok(());
            }
            CheckOutcome::Verified(r) => *r,
            CheckOutcome::Offline => {
                AgentLog::warn("update download failed: offline");
                self.set_state("error", Some(OFFLINE_ERROR.to_string()));
                sink.emit_now(UpdateProgress {
                    phase: "offline".to_string(),
                    done_bytes: 0,
                    total_bytes: 0,
                    error: Some(OFFLINE_ERROR.to_string()),
                });
                return Ok(());
            }
            CheckOutcome::Rejected(reason) => {
                AgentLog::warn(&format!("update download rejected: {reason}"));
                self.set_state("error", Some(reason.clone()));
                sink.emit_now(UpdateProgress {
                    phase: "failed".to_string(),
                    done_bytes: 0,
                    total_bytes: 0,
                    error: Some(reason),
                });
                return Ok(());
            }
            CheckOutcome::NoRelease => {
                let msg = "No update release found.".to_string();
                AgentLog::info("update download: no release found");
                self.set_state("idle", None);
                sink.emit_now(UpdateProgress {
                    phase: "failed".to_string(),
                    done_bytes: 0,
                    total_bytes: 0,
                    error: Some(msg),
                });
                return Ok(());
            }
            CheckOutcome::Unverifiable { .. } => {
                self.set_state("error", Some(NO_KEY_REASON.to_string()));
                sink.emit_now(UpdateProgress {
                    phase: "failed".to_string(),
                    done_bytes: 0,
                    total_bytes: 0,
                    error: Some(NO_KEY_REASON.to_string()),
                });
                return Ok(());
            }
        };

        if rel.version <= running {
            let msg = "No newer update is available.".to_string();
            AgentLog::info(&format!(
                "update download refused: latest={} not newer than running={running}",
                rel.version
            ));
            {
                let mut rt = self.runtime.lock().unwrap_or_else(|e| e.into_inner());
                rt.persisted.last_check = Some(now);
                rt.persisted.latest = Some(PersistedLatest {
                    version: rel.version.to_string(),
                    notes_url: rel.notes_url,
                    published_at: rel.published_at,
                    installer_bytes: Some(rel.manifest.size),
                });
                rt.state = "idle".to_string();
                rt.error = None;
                rt.install_error = false;
                let _ = save_state(&updates_state_path(&self.home), &rt.persisted);
            }
            sink.emit_now(UpdateProgress {
                phase: "failed".to_string(),
                done_bytes: 0,
                total_bytes: 0,
                error: Some(msg),
            });
            return Ok(());
        }

        let ver_str = rel.version.to_string();
        let updates_root = DataPaths::updates_dir(&self.home);
        if let Err(e) = fs::create_dir_all(&updates_root) {
            let msg = format!("Could not prepare updates folder: {e}");
            AgentLog::warn(&format!("update download failed: {msg}"));
            self.set_state("error", Some(msg.clone()));
            sink.emit_now(UpdateProgress {
                phase: "failed".to_string(),
                done_bytes: 0,
                total_bytes: rel.manifest.size,
                error: Some(msg),
            });
            return Ok(());
        }

        // Remove other version folders under cache\updates\ before starting.
        clean_other_version_dirs(&updates_root, &ver_str);

        let ver_dir = updates_root.join(&ver_str);
        if let Err(e) = fs::create_dir_all(&ver_dir) {
            let msg = format!("Could not create update folder: {e}");
            AgentLog::warn(&format!("update download failed: {msg}"));
            self.set_state("error", Some(msg.clone()));
            sink.emit_now(UpdateProgress {
                phase: "failed".to_string(),
                done_bytes: 0,
                total_bytes: rel.manifest.size,
                error: Some(msg),
            });
            return Ok(());
        }

        if let Err(e) = fs::write(ver_dir.join(UPDATE_JSON), &rel.manifest_bytes)
            .and_then(|()| fs::write(ver_dir.join(UPDATE_SIG), &rel.sig_bytes))
        {
            let msg = format!("Could not write update manifest: {e}");
            AgentLog::warn(&format!("update download failed: {msg}"));
            self.set_state("error", Some(msg.clone()));
            sink.emit_now(UpdateProgress {
                phase: "failed".to_string(),
                done_bytes: 0,
                total_bytes: rel.manifest.size,
                error: Some(msg),
            });
            return Ok(());
        }

        let installer_path = ver_dir.join(&rel.manifest.installer);
        let part_len = fs::metadata(part_path_for(&installer_path))
            .map(|m| m.len().min(rel.manifest.size))
            .unwrap_or(0);

        AgentLog::info(&format!(
            "update download start: version={ver_str}, size={}",
            rel.manifest.size
        ));
        self.set_state("downloading", None);
        sink.init_progress(part_len, rel.manifest.size);
        sink.emit_now(UpdateProgress {
            phase: "downloading".to_string(),
            done_bytes: part_len,
            total_bytes: rel.manifest.size,
            error: None,
        });

        let downloader = make_downloader(cfg.fast_retries);
        let cancel = || ct.is_cancelled();
        match downloader.download_file(
            &rel.installer_url,
            &installer_path,
            rel.manifest.size,
            &rel.manifest.sha256,
            &sink,
            &cancel,
        ) {
            Ok(()) => {
                sink.emit_now(UpdateProgress {
                    phase: "verifying".to_string(),
                    done_bytes: rel.manifest.size,
                    total_bytes: rel.manifest.size,
                    error: None,
                });
                {
                    let mut rt = self.runtime.lock().unwrap_or_else(|e| e.into_inner());
                    rt.persisted.last_check = Some(now);
                    rt.persisted.latest = Some(PersistedLatest {
                        version: ver_str.clone(),
                        notes_url: rel.notes_url,
                        published_at: rel.published_at,
                        installer_bytes: Some(rel.manifest.size),
                    });
                    rt.persisted.downloaded = Some(ver_str.clone());
                    rt.state = "ready".to_string();
                    rt.error = None;
                    rt.install_error = false;
                    let _ = save_state(&updates_state_path(&self.home), &rt.persisted);
                }
                AgentLog::info(&format!("update download done: version={ver_str}"));
                sink.emit_now(UpdateProgress {
                    phase: "done".to_string(),
                    done_bytes: rel.manifest.size,
                    total_bytes: rel.manifest.size,
                    error: None,
                });
            }
            Err(FetchError::Cancelled) => {
                AgentLog::info(&format!("update download cancelled: version={ver_str}"));
                let (done, total) = sink.snapshot();
                self.set_state("idle", None);
                sink.emit_now(UpdateProgress {
                    phase: "cancelled".to_string(),
                    done_bytes: done,
                    total_bytes: total,
                    error: None,
                });
            }
            Err(FetchError::Offline) => {
                AgentLog::warn(&format!("update download offline: version={ver_str}"));
                let (done, total) = sink.snapshot();
                self.set_state("error", Some(OFFLINE_ERROR.to_string()));
                sink.emit_now(UpdateProgress {
                    phase: "offline".to_string(),
                    done_bytes: done,
                    total_bytes: total,
                    error: Some(OFFLINE_ERROR.to_string()),
                });
            }
            Err(e) => {
                let msg = sanitize_fetch_error(&e);
                AgentLog::warn(&format!(
                    "update download failed: version={ver_str}, reason={msg}"
                ));
                let (done, total) = sink.snapshot();
                self.set_state("error", Some(msg.clone()));
                sink.emit_now(UpdateProgress {
                    phase: "failed".to_string(),
                    done_bytes: done,
                    total_bytes: total,
                    error: Some(msg),
                });
            }
        }
        Ok(())
    }

    /// Re-verifies the downloaded manifest, signature, and installer through a share-locked handle,
    /// then launches the installer with `/S`.
    pub fn apply(&self, auto_enabled: bool) -> Result<UpdateStatus, AgentError> {
        let cfg = self.effective_config();
        let Some(ref pub_pem) = cfg.public_key_pem else {
            return Err(AgentError::InvalidOperation(NO_KEY_REASON.to_string()));
        };
        // C7: apply must refuse while a model download is running
        if self.models_downloading.load(Ordering::Acquire) {
            return Err(AgentError::InvalidOperation(
                CommandProcessor::ERR_MODELS_RUNNING_FOR_UPDATE.to_string(),
            ));
        }
        let running = Version::parse(&cfg.current_version).map_err(|e| {
            AgentError::Other(format!("invalid running version '{}': {e}", cfg.current_version))
        })?;

        // D3: Test-and-set `applying` under the runtime lock before verifying/starting the
        // installer so two concurrent apply calls cannot both start an installer.
        let (downloaded_ver, mut applying_guard) = {
            let mut rt = self.runtime.lock().unwrap_or_else(|e| e.into_inner());
            if rt.state == "applying" {
                return Err(AgentError::InvalidOperation(
                    "Update is already being installed.".to_string(),
                ));
            }
            let Some(dl) = rt.persisted.downloaded.clone() else {
                return Err(AgentError::InvalidOperation(
                    "No downloaded update is ready to install.".to_string(),
                ));
            };
            let prev_state = std::mem::replace(&mut rt.state, "applying".to_string());
            let prev_error = rt.error.take();
            let prev_install_error = std::mem::replace(&mut rt.install_error, false);
            (
                dl,
                ApplyingGuard {
                    runtime: &self.runtime,
                    prev_state,
                    prev_error,
                    prev_install_error,
                    armed: true,
                },
            )
        };

        let target_ver = Version::parse(&downloaded_ver).map_err(|_| {
            AgentError::InvalidOperation("Downloaded update version is invalid.".to_string())
        })?;
        if target_ver <= running {
            return Err(AgentError::InvalidOperation(
                "Downloaded update is not newer than the installed version.".to_string(),
            ));
        }

        let ver_dir = DataPaths::updates_dir(&self.home).join(&downloaded_ver);
        let manifest_bytes = fs::read(ver_dir.join(UPDATE_JSON)).map_err(|_| {
            AgentError::InvalidOperation("Downloaded update manifest is missing.".to_string())
        })?;
        let sig_bytes = fs::read(ver_dir.join(UPDATE_SIG)).map_err(|_| {
            AgentError::InvalidOperation("Downloaded update signature is missing.".to_string())
        })?;
        if manifest_bytes.len() > MAX_MANIFEST_BYTES || sig_bytes.len() > MAX_SIG_BYTES {
            return Err(AgentError::InvalidOperation(
                "Downloaded update manifest exceeds size limit.".to_string(),
            ));
        }

        let manifest = verify_update_manifest(&manifest_bytes, &sig_bytes, pub_pem)
            .map_err(|e| AgentError::InvalidOperation(format!("Update verification failed: {e}")))?;

        let manifest_ver = Version::parse(&manifest.version).map_err(|_| {
            AgentError::InvalidOperation("Update manifest version is invalid.".to_string())
        })?;
        if manifest_ver != target_ver || manifest_ver <= running {
            return Err(AgentError::InvalidOperation(
                "Update manifest version is not valid for install.".to_string(),
            ));
        }
        if !is_valid_installer_name(&manifest.installer) || manifest.size > MAX_INSTALLER_BYTES {
            return Err(AgentError::InvalidOperation(
                "Update manifest installer metadata is invalid.".to_string(),
            ));
        }

        let installer_path = ver_dir.join(&manifest.installer);
        // Hold the file open with FILE_SHARE_READ only (no write or delete sharing) while hashing
        // and until after the process has started.
        let mut file = OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ.0)
            .open(&installer_path)
            .map_err(|_| {
                AgentError::InvalidOperation("Downloaded installer could not be opened.".to_string())
            })?;

        let meta = file.metadata().map_err(|_| {
            AgentError::InvalidOperation("Could not inspect downloaded installer.".to_string())
        })?;
        if !meta.is_file() || meta.len() != manifest.size {
            AgentLog::warn(&format!(
                "update apply refused: installer size mismatch for version={downloaded_ver}"
            ));
            return Err(AgentError::InvalidOperation(
                "Downloaded installer failed verification.".to_string(),
            ));
        }

        let actual_sha = hash_open_file(&mut file).map_err(|_| {
            AgentError::InvalidOperation("Could not verify downloaded installer.".to_string())
        })?;
        if !ct_eq_str(&actual_sha, &manifest.sha256) {
            AgentLog::warn(&format!(
                "update apply refused: installer hash mismatch for version={downloaded_ver}"
            ));
            return Err(AgentError::InvalidOperation(
                "Downloaded installer failed verification.".to_string(),
            ));
        }

        let launch_res = if let Some(ref hook) = cfg.launcher {
            hook(&installer_path, &["/S".to_string()])
        } else {
            launch_installer_detached(&installer_path, &ver_dir).map(Some)
        };
        drop(file);

        let proc_handle = match launch_res {
            Ok(h) => {
                applying_guard.armed = false;
                h
            }
            Err(e) => {
                applying_guard.armed = false;
                AgentLog::warn(&format!(
                    "update apply failed to start installer for version={downloaded_ver}: {e}"
                ));
                let mut rt = self.runtime.lock().unwrap_or_else(|e| e.into_inner());
                rt.state = "error".to_string();
                let err_msg = format!("Could not start the installer: {e}");
                rt.error = Some(err_msg);
                rt.install_error = true;
                rt.persisted.record_failure(&downloaded_ver, Utc::now());
                let _ = save_state(&updates_state_path(&self.home), &rt.persisted);
                return Err(AgentError::InvalidOperation(format!(
                    "Could not start the installer: {e}"
                )));
            }
        };

        AgentLog::info(&format!(
            "update apply started installer: version={downloaded_ver}"
        ));

        // C1: Watch the installer
        if let Some(h) = proc_handle {
            let runtime = Arc::clone(&self.runtime);
            let home = self.home.clone();
            let version = downloaded_ver.clone();
            let wrapper = ProcessHandle(h);
            let _ = std::thread::Builder::new()
                .name("br-installer-watcher".to_string())
                .spawn(move || {
                    // Move the whole Send wrapper in (closures capture fields disjointly).
                    let wrapper = wrapper;
                    let wait_res = unsafe { WaitForSingleObject(wrapper.0, INFINITE) };
                    if wait_res == WAIT_OBJECT_0 {
                        let mut exit_code = 0u32;
                        let ok = unsafe { GetExitCodeProcess(wrapper.0, &mut exit_code) };
                        if ok.is_ok() {
                            // C3: Read last-install.json so start-up does not count this exit again.
                            let report_utc = fs::read_to_string(
                                DataPaths::updates_dir(&home).join("last-install.json"),
                            )
                            .ok()
                            .and_then(|c| serde_json::from_str::<LastInstallReport>(&c).ok())
                            .filter(|r| r.version == version)
                            .map(|r| r.utc);

                            let mut rt = runtime.lock().unwrap_or_else(|e| e.into_inner());
                            if let Some(utc) = report_utc {
                                rt.persisted.last_install_handled_utc = Some(utc);
                            }
                            if exit_code == 0 {
                                AgentLog::info(&format!(
                                    "installer exited successfully (code 0) for {version}"
                                ));
                                rt.state = if rt.persisted.downloaded.is_some() {
                                    "ready".to_string()
                                } else {
                                    "idle".to_string()
                                };
                                rt.error = None;
                                rt.install_error = false;
                            } else {
                                AgentLog::warn(&format!(
                                    "installer exited with code {exit_code} for {version}"
                                ));
                                rt.state = "error".to_string();
                                rt.error = Some(format!(
                                    "Update could not be installed (setup exit code {exit_code})."
                                ));
                                rt.install_error = true;
                                rt.persisted.record_failure(&version, Utc::now());
                            }
                            let _ = save_state(&updates_state_path(&home), &rt.persisted);
                        }
                    }
                });
        }

        let rt = self.runtime.lock().unwrap_or_else(|e| e.into_inner());
        Ok(self.build_status(&cfg, &rt, auto_enabled))
    }

    /// One evaluation step of the auto-update scheduler (injectable time, idle, busy, and setting for tests).
    pub fn auto_step(
        &self,
        now: DateTime<Utc>,
        idle_ms: i64,
        is_busy: bool,
        auto_enabled: bool,
        ct: &CancelToken,
    ) -> AutoStepOutcome {
        if !auto_enabled || !self.has_public_key() || ct.is_cancelled() {
            return AutoStepOutcome::Disabled;
        }

        // C1: auto_step must not apply a version while state is applying
        {
            let rt = self.runtime.lock().unwrap_or_else(|e| e.into_inner());
            if rt.state == "applying" {
                return AutoStepOutcome::Deferred;
            }
        }

        let need_check = {
            let rt = self.runtime.lock().unwrap_or_else(|e| e.into_inner());
            match rt.persisted.last_check {
                None => true,
                Some(last) => now.signed_duration_since(last) >= ChronoDuration::hours(AUTO_CHECK_AGE_HOURS),
            }
        };

        if need_check && let Err(e) = self.check_at(now, auto_enabled, ct) {
            AgentLog::warn(&format!("auto-update check failed: {e}"));
            return AutoStepOutcome::Failed;
        }

        let status = self.status(auto_enabled);
        if !status.available || !status.can_install {
            return AutoStepOutcome::NoUpdate;
        }

        let latest_ver = status.latest_version.clone();
        let already_downloaded = latest_ver.is_some() && status.downloaded_version == latest_ver;

        // C1: auto_step must not re-apply failedVersion until failedAt + backoff
        {
            let rt = self.runtime.lock().unwrap_or_else(|e| e.into_inner());
            if let (Some(target), Some(failed_ver)) = (&latest_ver, &rt.persisted.failed_version)
                && target == failed_ver
                && let Some(failed_at) = rt.persisted.failed_at
            {
                let backoff = failure_backoff(rt.persisted.failed_count);
                if now < failed_at + backoff {
                    return AutoStepOutcome::Deferred;
                }
            }
        }
        if !already_downloaded {
            if is_busy {
                return AutoStepOutcome::Deferred;
            }
            let mut last_phase = String::new();
            if let Err(e) = self.download_at(now, &mut |p| last_phase = p.phase, ct) {
                AgentLog::warn(&format!("auto-update download refused: {e}"));
                return AutoStepOutcome::Deferred;
            }
            if last_phase != "done" {
                return AutoStepOutcome::Failed;
            }
        }

        if idle_ms < AUTO_IDLE_THRESHOLD_MS || is_busy {
            return if already_downloaded {
                AutoStepOutcome::Deferred
            } else {
                AutoStepOutcome::Downloaded
            };
        }

        match self.apply(auto_enabled) {
            Ok(_) => AutoStepOutcome::Applied,
            Err(e) => {
                AgentLog::warn(&format!("auto-update apply failed: {e}"));
                AutoStepOutcome::Failed
            }
        }
    }

    fn effective_config(&self) -> EffectiveConfig {
        if let Some(tc) = self
            .test_config
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
        {
            return EffectiveConfig {
                feed_url: tc.feed_url,
                allowed_prefix: tc.allowed_prefix,
                public_key_pem: tc.public_key_pem,
                current_version: tc
                    .current_version
                    .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_string()),
                fast_retries: tc.fast_retries,
                launcher: tc.launcher,
            };
        }
        EffectiveConfig {
            feed_url: PROD_FEED_URL.to_string(),
            allowed_prefix: PROD_ALLOWED_PREFIX.to_string(),
            public_key_pem: br_integrity::embedded_public_key_pem().map(str::to_string),
            current_version: env!("CARGO_PKG_VERSION").to_string(),
            fast_retries: false,
            launcher: None,
        }
    }

    fn set_state(&self, state: &str, error: Option<String>) {
        let mut rt = self.runtime.lock().unwrap_or_else(|e| e.into_inner());
        rt.state = state.to_string();
        rt.error = error;
        rt.install_error = false;
    }

    fn build_status(
        &self,
        cfg: &EffectiveConfig,
        rt: &RuntimeState,
        auto_enabled: bool,
    ) -> UpdateStatus {
        let can_install = cfg.public_key_pem.is_some();
        let reason = if can_install {
            None
        } else {
            Some(NO_KEY_REASON.to_string())
        };
        let running = Version::parse(&cfg.current_version).ok();
        let latest_version = rt.persisted.latest.as_ref().map(|l| l.version.clone());
        let available = match (&running, &latest_version) {
            (Some(cur), Some(lat)) => Version::parse(lat).is_ok_and(|v| v > *cur),
            _ => false,
        };
        let downloaded_version = match (&running, &rt.persisted.downloaded) {
            (Some(cur), Some(dl)) if Version::parse(dl).is_ok_and(|v| v > *cur) => Some(dl.clone()),
            _ => None,
        };
        UpdateStatus {
            current_version: cfg.current_version.clone(),
            latest_version,
            available,
            can_install,
            reason,
            notes_url: rt
                .persisted
                .latest
                .as_ref()
                .and_then(|l| l.notes_url.clone()),
            published_at: rt.persisted.latest.as_ref().and_then(|l| l.published_at),
            installer_bytes: rt.persisted.latest.as_ref().and_then(|l| l.installer_bytes),
            state: rt.state.clone(),
            downloaded_version,
            last_check: rt.persisted.last_check,
            auto: auto_enabled,
            error: rt.error.clone(),
        }
    }

    fn run_feed_check(&self, cfg: &EffectiveConfig, ct: &CancelToken) -> CheckOutcome {
        let Ok(running) = Version::parse(&cfg.current_version) else {
            return CheckOutcome::Rejected("Invalid installed version.".to_string());
        };
        let allow_prerelease = !running.pre.is_empty();
        let ua = format!("BetterRecall-Updater/{}", cfg.current_version);
        let feed_headers = [
            ("Accept", "application/vnd.github+json"),
            ("X-GitHub-Api-Version", "2022-11-28"),
            ("User-Agent", ua.as_str()),
        ];

        let cancel = || ct.is_cancelled();
        let dl = make_downloader(cfg.fast_retries);
        let feed_bytes = match dl.get_bytes_cancellable(
            &cfg.feed_url,
            MAX_FEED_BYTES,
            &feed_headers,
            &cancel,
        ) {
            Ok(b) => b,
            Err(FetchError::Cancelled) => return CheckOutcome::Cancelled,
            Err(FetchError::Offline) => return CheckOutcome::Offline,
            Err(FetchError::Http {
                status: 403 | 429, ..
            }) => return CheckOutcome::Rejected(RATE_LIMIT_ERROR.to_string()),
            Err(e) => return CheckOutcome::Rejected(sanitize_fetch_error(&e)),
        };

        let releases: Vec<GhRelease> = match serde_json::from_slice(&feed_bytes) {
            Ok(r) => r,
            Err(_) => {
                return CheckOutcome::Rejected("Release feed could not be parsed.".to_string());
            }
        };

        let mut best: Option<(Version, &GhRelease)> = None;
        for rel in &releases {
            if rel.draft {
                continue;
            }
            if rel.prerelease && !allow_prerelease {
                continue;
            }
            let raw_tag = rel
                .tag_name
                .strip_prefix('v')
                .unwrap_or(&rel.tag_name);
            let Ok(ver) = Version::parse(raw_tag) else {
                continue;
            };
            if !allow_prerelease && !ver.pre.is_empty() {
                continue;
            }
            let has_manifest = rel.assets.iter().any(|a| a.name == UPDATE_JSON);
            let has_sig = rel.assets.iter().any(|a| a.name == UPDATE_SIG);
            if !has_manifest || !has_sig {
                continue;
            }
            if best.as_ref().is_none_or(|(b, _)| ver > *b) {
                best = Some((ver, rel));
            }
        }

        let Some((tag_ver, rel)) = best else {
            return CheckOutcome::NoRelease;
        };

        let notes_url = rel
            .html_url
            .as_deref()
            .filter(|u| u.starts_with(&cfg.allowed_prefix))
            .map(str::to_string);
        let published_at = rel
            .published_at
            .as_deref()
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|dt| dt.with_timezone(&Utc));

        let Some(ref pub_pem) = cfg.public_key_pem else {
            return CheckOutcome::Unverifiable {
                version: tag_ver,
                notes_url,
                published_at,
            };
        };

        let Some(manifest_asset) = rel.assets.iter().find(|a| a.name == UPDATE_JSON) else {
            return CheckOutcome::Rejected("Release is missing update.json.".to_string());
        };
        let Some(sig_asset) = rel.assets.iter().find(|a| a.name == UPDATE_SIG) else {
            return CheckOutcome::Rejected("Release is missing update.sig.".to_string());
        };

        if !manifest_asset
            .browser_download_url
            .starts_with(&cfg.allowed_prefix)
            || !sig_asset
                .browser_download_url
                .starts_with(&cfg.allowed_prefix)
        {
            return CheckOutcome::Rejected(
                "Release asset URL is outside the allowed release host.".to_string(),
            );
        }

        let asset_headers = [("User-Agent", ua.as_str())];
        let manifest_bytes = match dl.get_bytes_cancellable(
            &manifest_asset.browser_download_url,
            MAX_MANIFEST_BYTES,
            &asset_headers,
            &cancel,
        ) {
            Ok(b) => b,
            Err(FetchError::Cancelled) => return CheckOutcome::Cancelled,
            Err(FetchError::Offline) => return CheckOutcome::Offline,
            Err(FetchError::Http {
                status: 403 | 429, ..
            }) => return CheckOutcome::Rejected(RATE_LIMIT_ERROR.to_string()),
            Err(e) => return CheckOutcome::Rejected(sanitize_fetch_error(&e)),
        };
        let sig_bytes = match dl.get_bytes_cancellable(
            &sig_asset.browser_download_url,
            MAX_SIG_BYTES,
            &asset_headers,
            &cancel,
        ) {
            Ok(b) => b,
            Err(FetchError::Cancelled) => return CheckOutcome::Cancelled,
            Err(FetchError::Offline) => return CheckOutcome::Offline,
            Err(FetchError::Http {
                status: 403 | 429, ..
            }) => return CheckOutcome::Rejected(RATE_LIMIT_ERROR.to_string()),
            Err(e) => return CheckOutcome::Rejected(sanitize_fetch_error(&e)),
        };

        let manifest = match verify_update_manifest(&manifest_bytes, &sig_bytes, pub_pem) {
            Ok(m) => m,
            Err(e) => {
                return CheckOutcome::Rejected(format!("Update signature check failed: {e}"));
            }
        };

        let Ok(manifest_ver) = Version::parse(&manifest.version) else {
            return CheckOutcome::Rejected("Update manifest version is not valid semver.".to_string());
        };
        if manifest_ver != tag_ver {
            return CheckOutcome::Rejected(
                "Update manifest version does not match the release tag.".to_string(),
            );
        }
        if !is_valid_installer_name(&manifest.installer) {
            return CheckOutcome::Rejected(
                "Update manifest installer filename is invalid.".to_string(),
            );
        }
        if manifest.size > MAX_INSTALLER_BYTES {
            return CheckOutcome::Rejected(
                "Update installer exceeds the 4 GiB size limit.".to_string(),
            );
        }

        let Some(installer_asset) = rel.assets.iter().find(|a| a.name == manifest.installer) else {
            return CheckOutcome::Rejected(
                "Release does not contain the installer named in the manifest.".to_string(),
            );
        };
        if installer_asset.size != manifest.size {
            return CheckOutcome::Rejected(
                "Installer asset size does not match the signed manifest.".to_string(),
            );
        }
        if !installer_asset
            .browser_download_url
            .starts_with(&cfg.allowed_prefix)
        {
            return CheckOutcome::Rejected(
                "Installer download URL is outside the allowed release host.".to_string(),
            );
        }

        CheckOutcome::Verified(Box::new(VerifiedRelease {
            version: manifest_ver,
            notes_url,
            published_at,
            manifest,
            manifest_bytes,
            sig_bytes,
            installer_url: installer_asset.browser_download_url.clone(),
        }))
    }
}

// D3: Restores runtime state if `apply` fails before the installer starts.
struct ApplyingGuard<'a> {
    runtime: &'a Mutex<RuntimeState>,
    prev_state: String,
    prev_error: Option<String>,
    prev_install_error: bool,
    armed: bool,
}

impl Drop for ApplyingGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            let mut rt = self.runtime.lock().unwrap_or_else(|e| e.into_inner());
            rt.state = std::mem::take(&mut self.prev_state);
            rt.error = self.prev_error.take();
            rt.install_error = self.prev_install_error;
        }
    }
}

struct FlagGuard<'a> {
    flag: &'a AtomicBool,
}

impl Drop for FlagGuard<'_> {
    fn drop(&mut self) {
        self.flag.store(false, Ordering::Release);
    }
}

struct UpdateProgressSink<'a> {
    emit: RefCell<&'a mut dyn FnMut(UpdateProgress)>,
    done: RefCell<u64>,
    total: RefCell<u64>,
    last_emit: RefCell<Option<Instant>>,
}

impl<'a> UpdateProgressSink<'a> {
    fn new(emit: &'a mut dyn FnMut(UpdateProgress)) -> Self {
        Self {
            emit: RefCell::new(emit),
            done: RefCell::new(0),
            total: RefCell::new(0),
            last_emit: RefCell::new(None),
        }
    }

    fn init_progress(&self, done: u64, total: u64) {
        *self.done.borrow_mut() = done;
        *self.total.borrow_mut() = total;
    }

    fn emit_now(&self, p: UpdateProgress) {
        (self.emit.borrow_mut())(p);
        *self.last_emit.borrow_mut() = Some(Instant::now());
    }

    fn snapshot(&self) -> (u64, u64) {
        (*self.done.borrow(), *self.total.borrow())
    }
}

impl FetchProgress for UpdateProgressSink<'_> {
    fn total(&self, bytes: u64) {
        if bytes > 0 {
            *self.total.borrow_mut() = bytes;
        }
    }

    fn bytes(&self, delta: u64) {
        let done = {
            let mut d = self.done.borrow_mut();
            *d = d.saturating_add(delta);
            *d
        };
        let due = self
            .last_emit
            .borrow()
            .is_none_or(|t| t.elapsed() >= PROGRESS_MIN_INTERVAL);
        if due {
            let total = *self.total.borrow();
            self.emit_now(UpdateProgress {
                phase: "downloading".to_string(),
                done_bytes: done.min(total),
                total_bytes: total,
                error: None,
            });
        }
    }
}

fn make_downloader(fast_retries: bool) -> Downloader {
    if fast_retries {
        Downloader::new().with_retry_delays(vec![Duration::from_millis(10)])
    } else {
        Downloader::new()
    }
}

fn updates_state_path(home: &Path) -> PathBuf {
    DataPaths::updates_dir(home).join(STATE_FILE_NAME)
}

fn load_state(path: &Path) -> PersistedState {
    match fs::read_to_string(path) {
        Ok(raw) => serde_json::from_str(&raw).unwrap_or_default(),
        Err(_) => PersistedState::default(),
    }
}

fn save_state(path: &Path, state: &PersistedState) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let bytes = serde_json::to_vec(state)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let mut tmp_os = path.as_os_str().to_os_string();
    tmp_os.push(".tmp");
    let tmp = PathBuf::from(tmp_os);
    {
        let mut f = File::create(&tmp)?;
        f.write_all(&bytes)?;
        f.sync_all()?;
    }
    if path.exists() {
        win32::replace_file(&tmp, path)
    } else {
        win32::move_replace(&tmp, path)
    }
}

fn clean_other_version_dirs(updates_root: &Path, keep_version: &str) {
    let Ok(entries) = fs::read_dir(updates_root) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(ft) = entry.file_type() else {
            continue;
        };
        if !ft.is_dir() {
            continue;
        }
        let name = entry.file_name();
        if name.to_string_lossy().eq_ignore_ascii_case(keep_version) {
            continue;
        }
        let _ = fs::remove_dir_all(entry.path());
    }
}

fn part_path_for(dest: &Path) -> PathBuf {
    let mut os = dest.as_os_str().to_os_string();
    os.push(".part");
    PathBuf::from(os)
}

fn is_valid_installer_name(name: &str) -> bool {
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.contains('\0')
        || name.contains('/')
        || name.contains('\\')
        || name.contains(':')
    {
        return false;
    }
    br_contracts::strip_suffix_ci(name, ".exe").is_some_and(|stem| !stem.is_empty())
}

fn hash_open_file(file: &mut File) -> io::Result<String> {
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; HASH_BUF_LEN];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let digest = hasher.finalize();
    let mut out = String::with_capacity(64);
    for b in digest {
        let _ = write!(&mut out, "{b:02x}");
    }
    Ok(out)
}

fn ct_eq_str(a: &str, b: &str) -> bool {
    let ab = a.as_bytes();
    let bb = b.as_bytes();
    if ab.len() != bb.len() {
        return false;
    }
    let mut diff = 0u8;
    for (&x, &y) in ab.iter().zip(bb.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

fn sanitize_fetch_error(err: &FetchError) -> String {
    match err {
        FetchError::Http {
            status: 403 | 429, ..
        } => RATE_LIMIT_ERROR.to_string(),
        FetchError::Http { status, .. } => format!("Download failed (HTTP {status})."),
        FetchError::Verify { .. } => "Downloaded installer failed verification.".to_string(),
        FetchError::NotEnoughSpace { .. } => "Not enough free disk space.".to_string(),
        FetchError::Offline => OFFLINE_ERROR.to_string(),
        FetchError::Cancelled => "Download cancelled.".to_string(),
        FetchError::Io { .. } => "Transfer or disk error while downloading update.".to_string(),
        FetchError::Catalog(msg) => format!("Invalid update data: {msg}"),
        FetchError::InUse => "Another download is writing this file.".to_string(),
    }
}

fn to_wide_nul(s: &str) -> Vec<u16> {
    std::ffi::OsStr::new(s)
        .encode_wide()
        .chain(Some(0))
        .collect()
}

fn launch_installer_detached(installer_path: &Path, work_dir: &Path) -> io::Result<HANDLE> {
    let app_w = to_wide_nul(&installer_path.to_string_lossy());
    let dir_w = to_wide_nul(&work_dir.to_string_lossy());
    let cmd_str = format!("\"{}\" /S", installer_path.display());

    let base_flags = DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP;
    let breakaway_flags = base_flags | CREATE_BREAKAWAY_FROM_JOB;

    match spawn_with_flags(&app_w, &cmd_str, &dir_w, breakaway_flags) {
        Ok(h) => Ok(h),
        Err(e) if e.raw_os_error() == Some(ERROR_ACCESS_DENIED.0 as i32) => {
            spawn_with_flags(&app_w, &cmd_str, &dir_w, base_flags)
        }
        Err(e) => Err(e),
    }
}

/// `windows::core::Error` carries an HRESULT; `HRESULT_FROM_WIN32` errors (facility 7) map back to
/// the Win32 code so `ERROR_ACCESS_DENIED` (5) can be recognised.
fn win32_code_from_hresult(hr: i32) -> i32 {
    let u = hr as u32;
    if u & 0xFFFF_0000 == 0x8007_0000 {
        (u & 0xFFFF) as i32
    } else {
        hr
    }
}

fn spawn_with_flags(
    app_w: &[u16],
    cmd_str: &str,
    dir_w: &[u16],
    flags: PROCESS_CREATION_FLAGS,
) -> io::Result<HANDLE> {
    let mut cmd_w = to_wide_nul(cmd_str);
    let si = STARTUPINFOW {
        cb: std::mem::size_of::<STARTUPINFOW>() as u32,
        ..Default::default()
    };
    let mut pi = PROCESS_INFORMATION::default();
    // SAFETY: All wide buffers are NUL-terminated and live for the duration of CreateProcessW.
    unsafe {
        CreateProcessW(
            PCWSTR(app_w.as_ptr()),
            Some(PWSTR(cmd_w.as_mut_ptr())),
            None,
            None,
            false,
            flags,
            None,
            PCWSTR(dir_w.as_ptr()),
            &si,
            &mut pi,
        )
        .map_err(|e| io::Error::from_raw_os_error(win32_code_from_hresult(e.code().0)))?;
        let _ = CloseHandle(pi.hThread);
        Ok(pi.hProcess)
    }
}
