// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::windows::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;

use aes_gcm::aead::AeadInPlace;
use aes_gcm::{Aes256Gcm, KeyInit, Nonce, Tag};
use argon2::{Algorithm, Argon2, Params, Version};
use br_contracts::{DataPaths, Settings, full_path};
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use sha2::{Digest, Sha256};
use zeroize::{Zeroize, Zeroizing};

use crate::crypto::{CryptoError, HelloInterop, SecureKey, UnlockPassword};
use crate::log::Log;
use crate::win32;

type SettingsFn = Arc<dyn Fn() -> Settings + Send + Sync>;
type ClockFn = Arc<dyn Fn() -> DateTime<Utc> + Send + Sync>;

/// Random 256-bit data key, wrapped in `keys\datakey.bin` by one of three modes:
/// `tpm` (RSA-2048, non-exportable, Microsoft Platform Crypto Provider, OAEP-SHA1),
/// `passphrase` (Argon2id 64 MiB, 3 passes, 4 lanes, AES-256-GCM wrap), or `dpapi` (CurrentUser).
/// File layout: `"BRK1"`, mode byte (`1`/`2`/`3`), mode payload.
pub struct KeyVault {
    key_file: PathBuf,
    home: PathBuf,
    settings: SettingsFn,
    tpm_key_name: String,
    gate: Mutex<VaultState>,
    loaded_cv: Condvar,
    hello_available: Arc<AtomicBool>,
    hello_checked: Arc<AtomicBool>,
}

struct VaultState {
    key: Option<SecureKey>,
    file_mode: Option<&'static str>,
    loaded_gen: u64,
    pass_failures: i32,
    pass_locked_until_ms: i64,
    now: ClockFn,
}

impl KeyVault {
    const MODE_INVALID: &'static str = "invalid";
    pub const MODE_TPM: &'static str = "tpm";
    pub const MODE_PASSPHRASE: &'static str = "passphrase";
    pub const MODE_DPAPI: &'static str = "dpapi";

    pub const TPM_PROVIDER: &'static str = "Microsoft Platform Crypto Provider";
    pub const LEGACY_TPM_KEY_NAME: &'static str = "BetterRecall.DataKeyWrap";
    const MAGIC: [u8; 4] = *b"BRK1";
    const ARGON_MEMORY_KIB: i32 = 65_536;
    const ARGON_PASSES: i32 = 3;
    const ARGON_LANES: i32 = 4;
    const PASSPHRASE_PAYLOAD: usize = 12 + 16 + 12 + 32 + 16;

    pub const PASSPHRASE_MIN_LENGTH: usize = 8;
    pub const PASSPHRASE_MAX_LENGTH: usize = 1024;

    pub fn new(
        home: &Path,
        settings: impl Fn() -> Settings + Send + Sync + 'static,
        tpm_key_name: Option<String>,
        probe_hello: bool,
    ) -> Self {
        let key_file = DataPaths::key_file(home);
        let tpm_name = tpm_key_name.unwrap_or_else(|| Self::tpm_key_name_for(home));
        let file_mode = match read_file_mode(&key_file) {
            Ok(m) => m,
            Err(why) => {
                Log::error_err("key file damaged", &why);
                Some(Self::MODE_INVALID)
            }
        };
        let hello_available = Arc::new(AtomicBool::new(false));
        let hello_checked = Arc::new(AtomicBool::new(!probe_hello));
        if probe_hello {
            let avail = hello_available.clone();
            let checked = hello_checked.clone();
            let _ = thread::Builder::new()
                .name("BetterRecall.HelloProbe".to_string())
                .spawn(move || {
                    avail.store(HelloInterop::check_available(), Ordering::Release);
                    checked.store(true, Ordering::Release);
                });
        }
        Self {
            key_file,
            home: home.to_path_buf(),
            settings: Arc::new(settings),
            tpm_key_name: tpm_name,
            gate: Mutex::new(VaultState {
                key: None,
                file_mode,
                loaded_gen: 0,
                pass_failures: 0,
                pass_locked_until_ms: 0,
                now: Arc::new(Utc::now),
            }),
            loaded_cv: Condvar::new(),
            hello_available,
            hello_checked,
        }
    }

    /// Sets the clock used by passphrase rate limiting (tests).
    pub fn set_now_for_test(&self, now: impl Fn() -> DateTime<Utc> + Send + Sync + 'static) {
        self.gate.lock().expect("vault gate").now = Arc::new(now);
    }

    /// Sets Windows Hello availability for tests built with `probe_hello = false`.
    pub fn set_hello_available_for_test(&self, available: bool) {
        self.hello_available.store(available, Ordering::Release);
        self.hello_checked.store(true, Ordering::Release);
    }

    pub fn hello_available(&self) -> bool {
        self.hello_available.load(Ordering::Acquire)
    }

    pub fn hello_checked(&self) -> bool {
        self.hello_checked.load(Ordering::Acquire)
    }

    /// Mode of the key file when it exists, else the mode a new key would use.
    pub fn protection(&self) -> &'static str {
        let st = self.gate.lock().expect("vault gate");
        st.file_mode
            .unwrap_or_else(|| self.resolve_new_mode(&(self.settings)().security.key_protection, false))
    }

    pub fn key_loaded(&self) -> bool {
        self.gate.lock().expect("vault gate").key.is_some()
    }

    /// Blocks until the data key is loaded (or `destroy_key` wakes waiters) or `timeout` elapses.
    pub fn wait_loaded(&self, timeout: Duration) -> bool {
        let mut st = self.gate.lock().expect("vault gate");
        if st.key.is_some() {
            return true;
        }
        let start_gen = st.loaded_gen;
        let res = self
            .loaded_cv
            .wait_timeout_while(st, timeout, |s| s.key.is_none() && s.loaded_gen == start_gen);
        match res {
            Ok((guard, _)) => {
                st = guard;
                st.key.is_some()
            }
            Err(_) => false,
        }
    }

    /// True when the key file is passphrase-wrapped, or no key exists and passphrase mode is configured.
    pub fn needs_passphrase(&self) -> bool {
        let st = self.gate.lock().expect("vault gate");
        if st.key.is_some() {
            return false;
        }
        if st.file_mode.is_none() && self.data_exists() {
            return false;
        }
        let effective = st.file_mode.or_else(|| {
            if (self.settings)().security.key_protection == Self::MODE_PASSPHRASE {
                Some(Self::MODE_PASSPHRASE)
            } else {
                None
            }
        });
        effective == Some(Self::MODE_PASSPHRASE)
    }

    /// Seconds until the next passphrase try is accepted, `0` when tries are open.
    pub fn passphrase_retry_after_seconds(&self) -> i32 {
        let st = self.gate.lock().expect("vault gate");
        let wait_ms = st.pass_locked_until_ms - (st.now)().timestamp_millis();
        if wait_ms > 0 {
            ((wait_ms as f64) / 1000.0).ceil() as i32
        } else {
            0
        }
    }

    /// The next `unlock_with_passphrase` would set a new passphrase (create the key, or re-wrap a tpm/dpapi key).
    pub fn would_set_passphrase(&self) -> bool {
        let st = self.gate.lock().expect("vault gate");
        if st.key.is_some() {
            st.file_mode != Some(Self::MODE_PASSPHRASE)
                && (self.settings)().security.key_protection == Self::MODE_PASSPHRASE
        } else {
            st.file_mode.is_none() && !self.data_exists()
        }
    }

    /// Rule for a passphrase that creates the key or re-wraps it (B14).
    pub fn valid_new_passphrase(p: &str) -> bool {
        let len = p.encode_utf16().count();
        (Self::PASSPHRASE_MIN_LENGTH..=Self::PASSPHRASE_MAX_LENGTH).contains(&len)
            && !p.trim().is_empty()
    }

    /// TPM key name for a data folder: `"BetterRecall.DataKeyWrap."` + 16 uppercase hex of SHA-256 over full upper-case path.
    pub fn tpm_key_name_for(home: &Path) -> String {
        let full = full_path(home).unwrap_or_else(|_| home.to_path_buf());
        let trimmed = full
            .to_string_lossy()
            .trim_end_matches(['\\', '/'])
            .to_uppercase();
        let digest = Sha256::digest(trimmed.as_bytes());
        let mut hex = String::with_capacity(16);
        for &b in &digest[..8] {
            use std::fmt::Write;
            let _ = write!(&mut hex, "{b:02X}");
        }
        format!("{}.{hex}", Self::LEGACY_TPM_KEY_NAME)
    }

    /// The TPM key this folder may delete: only its own name, never the legacy shared name.
    pub fn owned_tpm_key(own_name: &str) -> Option<&str> {
        if own_name == Self::LEGACY_TPM_KEY_NAME {
            None
        } else {
            Some(own_name)
        }
    }

    /// TPM key name stored in a tpm-mode key file; `None` for other modes or a short file.
    pub fn tpm_name_in_file(file: &[u8]) -> Option<String> {
        if file.len() < 7 || file[..4] != Self::MAGIC || file[4] != 1 {
            return None;
        }
        let name_len = u16::from_le_bytes([file[5], file[6]]) as usize;
        if file.len() < 7 + name_len {
            return None;
        }
        String::from_utf8(file[7..7 + name_len].to_vec()).ok()
    }

    pub fn tpm_transient(hr: i32) -> bool {
        win32::tpm_transient(hr)
    }

    /// Loads the key without any prompt: unwraps a tpm or dpapi file, or creates a new key when none exists
    /// and the configured mode is not passphrase. Returns `Ok(false)` when a passphrase is needed.
    pub fn try_load_silent(&self) -> Result<bool, CryptoError> {
        let mut st = self.gate.lock().expect("vault gate");
        if st.key.is_some() {
            return Ok(true);
        }
        let s = (self.settings)();
        let iso = s.security.memory_isolation;
        match st.file_mode {
            None => {
                if self.data_exists() {
                    return Err(CryptoError::KeyLost("The key file is missing.".to_string()));
                }
                let mode = self.resolve_new_mode(&s.security.key_protection, true);
                if mode == Self::MODE_PASSPHRASE {
                    return Ok(false);
                }
                let key = SecureKey::random(iso)?;
                self.write_wrapped(&key, mode, None)?;
                self.set_loaded_locked(&mut st, key, mode);
                Log::info(&format!("data key created, protection {mode}"));
                Ok(true)
            }
            Some(Self::MODE_INVALID) => {
                Err(CryptoError::KeyLost("The key file is damaged.".to_string()))
            }
            Some(Self::MODE_PASSPHRASE) => Ok(false),
            Some(mode) => {
                let bytes = fs::read(&self.key_file)?;
                let loaded = if mode == Self::MODE_TPM {
                    unwrap_tpm(&bytes, iso)?
                } else {
                    unwrap_dpapi(&bytes, iso)?
                };
                self.set_loaded_locked(&mut st, loaded, mode);
                Log::info(&format!("data key loaded, protection {mode}"));
                if mode == Self::MODE_TPM
                    && Self::tpm_name_in_file(&bytes).as_deref() != Some(self.tpm_key_name.as_str())
                    && let Some(ref k) = st.key
                {
                    match self.write_wrapped(k, Self::MODE_TPM, None) {
                        Ok(()) => Log::info("data key re-wrapped under this folder's TPM key"),
                        Err(e) => Log::warn_err(
                            "TPM key rename failed, old wrap kept",
                            &e.to_string(),
                        ),
                    }
                }
                self.migrate_if_needed_locked(&mut st);
                Ok(true)
            }
        }
    }

    pub fn unlock_with_passphrase(&self, passphrase: &str) -> bool {
        if passphrase.is_empty() {
            return false;
        }
        let mut st = self.gate.lock().expect("vault gate");
        if (st.now)().timestamp_millis() < st.pass_locked_until_ms {
            return false;
        }
        let s = (self.settings)();
        if st.key.is_some() {
            if st.file_mode == Some(Self::MODE_PASSPHRASE) {
                let Ok(bytes) = fs::read(&self.key_file) else {
                    return false;
                };
                let ok = unwrap_passphrase(&bytes, passphrase, false)
                    .ok()
                    .flatten()
                    .is_some();
                note_passphrase_locked(&mut st, ok);
                return ok;
            }
            if s.security.key_protection == Self::MODE_PASSPHRASE {
                if !Self::valid_new_passphrase(passphrase) {
                    return false;
                }
                let was_tpm = st.file_mode == Some(Self::MODE_TPM);
                let res = st
                    .key
                    .as_ref()
                    .ok_or_else(|| CryptoError::InvalidState("No key".to_string()))
                    .and_then(|k| self.write_wrapped(k, Self::MODE_PASSPHRASE, Some(passphrase)));
                if res.is_err() {
                    return false;
                }
                st.file_mode = Some(Self::MODE_PASSPHRASE);
                if was_tpm {
                    self.delete_own_tpm_key();
                }
                Log::info("data key re-wrapped with passphrase");
                return true;
            }
            return false;
        }

        if st.file_mode.is_none() {
            if self.data_exists() || !Self::valid_new_passphrase(passphrase) {
                return false;
            }
            let Ok(key) = SecureKey::random(s.security.memory_isolation) else {
                return false;
            };
            if self
                .write_wrapped(&key, Self::MODE_PASSPHRASE, Some(passphrase))
                .is_err()
            {
                return false;
            }
            self.set_loaded_locked(&mut st, key, Self::MODE_PASSPHRASE);
            Log::info("data key created, protection passphrase");
            return true;
        }

        if st.file_mode != Some(Self::MODE_PASSPHRASE) {
            return false;
        }
        let Ok(bytes) = fs::read(&self.key_file) else {
            return false;
        };
        let unwrapped = unwrap_passphrase(&bytes, passphrase, s.security.memory_isolation)
            .ok()
            .flatten();
        note_passphrase_locked(&mut st, unwrapped.is_some());
        let Some(key) = unwrapped else {
            Log::info("passphrase unlock failed");
            return false;
        };
        self.set_loaded_locked(&mut st, key, Self::MODE_PASSPHRASE);
        Log::info("data key unlocked with passphrase");
        self.migrate_if_needed_locked(&mut st);
        true
    }

    /// Re-wraps a loaded key when `security.keyProtection` names `tpm` or `dpapi` and the file uses another mode.
    pub fn migrate_if_needed(&self) {
        let mut st = self.gate.lock().expect("vault gate");
        self.migrate_if_needed_locked(&mut st);
    }

    fn migrate_if_needed_locked(&self, st: &mut VaultState) {
        let Some(ref key) = st.key else {
            return;
        };
        let want_str = (self.settings)().security.key_protection;
        let want: &'static str = match want_str.as_str() {
            Self::MODE_TPM => Self::MODE_TPM,
            Self::MODE_DPAPI => Self::MODE_DPAPI,
            _ => return,
        };
        if st.file_mode == Some(want) {
            return;
        }
        let prev = st.file_mode;
        match self.write_wrapped(key, want, None) {
            Ok(()) => {
                Log::info(&format!(
                    "data key re-wrapped from {} to {want}",
                    prev.unwrap_or("none")
                ));
                if prev == Some(Self::MODE_TPM) {
                    self.delete_own_tpm_key();
                }
                st.file_mode = Some(want);
            }
            Err(e) => {
                Log::warn_err(
                    &format!("re-wrap to {want} failed, {} kept", prev.unwrap_or("none")),
                    &e.to_string(),
                );
            }
        }
    }

    pub fn verify_user(&self, hwnd: isize, reason: &str, cancel: Option<&AtomicBool>) -> bool {
        let msg = if reason.trim().is_empty() {
            "Unlock BetterRecall"
        } else {
            reason
        };
        HelloInterop::verify(hwnd, msg, cancel)
    }

    /// Overwrites and deletes `keys\datakey.bin`, deletes this folder's own TPM key, and zeroes key memory.
    pub fn destroy_key(&self) -> Result<(), CryptoError> {
        let mut st = self.gate.lock().expect("vault gate");
        if self.key_file.exists() {
            let len = fs::metadata(&self.key_file)?.len() as usize;
            let mut junk = vec![0u8; len.max(64)];
            win32::fill_random(&mut junk);
            {
                let mut f = OpenOptions::new()
                    .write(true)
                    .share_mode(0)
                    .open(&self.key_file)?;
                f.write_all(&junk)?;
                f.sync_all()?;
            }
            fs::remove_file(&self.key_file)?;
        }
        self.delete_own_tpm_key();
        st.key = None;
        st.file_mode = None;
        st.loaded_gen = st.loaded_gen.wrapping_add(1);
        self.loaded_cv.notify_all();
        Log::info("data key destroyed");
        Ok(())
    }

    /// HKDF-SHA256 subkey of the data key. Fails when the key is not loaded.
    pub fn derive(&self, label: &str) -> Result<Zeroizing<Vec<u8>>, CryptoError> {
        let st = self.gate.lock().expect("vault gate");
        let Some(ref k) = st.key else {
            return Err(CryptoError::InvalidState(
                "Data key not loaded.".to_string(),
            ));
        };
        k.derive(label, 32)
    }

    /// Lengths and parameters of a key file payload fit its mode.
    pub fn payload_valid(file: &[u8], mode: &str) -> bool {
        let p = &file[file.len().min(5)..];
        match mode {
            Self::MODE_TPM => {
                if p.len() < 2 {
                    return false;
                }
                let name_len = u16::from_le_bytes([p[0], p[1]]) as usize;
                if name_len == 0 || p.len() < 2 + name_len + 4 {
                    return false;
                }
                let len = i32::from_le_bytes(
                    p[2 + name_len..6 + name_len].try_into().unwrap_or([0; 4]),
                );
                len > 0 && (len as usize) <= p.len() - 6 - name_len
            }
            Self::MODE_DPAPI => {
                if p.len() < 4 {
                    return false;
                }
                let len = i32::from_le_bytes(p[0..4].try_into().unwrap_or([0; 4]));
                len > 0 && (len as usize) <= p.len() - 4
            }
            Self::MODE_PASSPHRASE => {
                if p.len() < Self::PASSPHRASE_PAYLOAD {
                    return false;
                }
                let mem = i32::from_le_bytes(p[0..4].try_into().unwrap_or([0; 4]));
                let passes = i32::from_le_bytes(p[4..8].try_into().unwrap_or([0; 4]));
                let lanes = i32::from_le_bytes(p[8..12].try_into().unwrap_or([0; 4]));
                (8192..=4 * 1024 * 1024).contains(&mem)
                    && (1..=64).contains(&passes)
                    && (1..=64).contains(&lanes)
            }
            _ => false,
        }
    }

    /// Argon2id KDF (RFC 9106 v1.3, 32-byte output).
    pub fn argon(
        passphrase: &str,
        salt: &[u8],
        mem_kib: i32,
        passes: i32,
        lanes: i32,
    ) -> Result<Zeroizing<Vec<u8>>, CryptoError> {
        let params = Params::new(mem_kib as u32, passes as u32, lanes as u32, Some(32))
            .map_err(|e| CryptoError::Cryptographic(format!("Argon2 params invalid: {e}")))?;
        let argon2 = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
        let mut out = Zeroizing::new(vec![0u8; 32]);
        argon2
            .hash_password_into(passphrase.as_bytes(), salt, &mut out)
            .map_err(|e| CryptoError::Cryptographic(format!("Argon2id failed: {e}")))?;
        Ok(out)
    }

    fn set_loaded_locked(&self, st: &mut VaultState, key: SecureKey, mode: &'static str) {
        st.key = Some(key);
        st.file_mode = Some(mode);
        st.loaded_gen = st.loaded_gen.wrapping_add(1);
        self.loaded_cv.notify_all();
    }

    fn data_exists(&self) -> bool {
        if DataPaths::store_db(&self.home).exists() {
            return true;
        }
        let kf = DataPaths::keyframes_dir(&self.home);
        if !kf.exists() {
            return false;
        }
        match fs::read_dir(&kf) {
            Ok(entries) => entries.filter_map(|e| e.ok()).any(|e| {
                e.path()
                    .extension()
                    .and_then(|x| x.to_str())
                    .is_some_and(|x| x.eq_ignore_ascii_case("db"))
            }),
            Err(_) => true,
        }
    }

    fn delete_own_tpm_key(&self) {
        let Some(name) = Self::owned_tpm_key(&self.tpm_key_name) else {
            return;
        };
        if let Err(hr) = win32::tpm_delete_key(name) {
            Log::debug(&format!("TPM key delete skipped: 0x{:08X}", hr as u32));
        }
    }

    fn resolve_new_mode(&self, configured: &str, probe: bool) -> &'static str {
        match configured {
            Self::MODE_TPM => return Self::MODE_TPM,
            Self::MODE_PASSPHRASE => return Self::MODE_PASSPHRASE,
            Self::MODE_DPAPI => return Self::MODE_DPAPI,
            _ => {}
        }
        if !probe {
            return if win32::tpm_provider_available() {
                Self::MODE_TPM
            } else {
                Self::MODE_DPAPI
            };
        }
        match win32::tpm_ensure_key(&self.tpm_key_name) {
            Ok(()) => Self::MODE_TPM,
            Err(hr) => {
                Log::info(&format!(
                    "TPM not usable (0x{:08X}), dpapi chosen",
                    hr as u32
                ));
                Self::MODE_DPAPI
            }
        }
    }

    fn write_wrapped(
        &self,
        key: &SecureKey,
        mode: &str,
        passphrase: Option<&str>,
    ) -> Result<(), CryptoError> {
        let payload = match mode {
            Self::MODE_TPM => self.wrap_tpm(key)?,
            Self::MODE_DPAPI => key.use_key(dpapi_wrap)??,
            Self::MODE_PASSPHRASE => {
                let pw = passphrase
                    .ok_or_else(|| CryptoError::InvalidArgument("Passphrase required.".to_string()))?;
                wrap_passphrase(key, pw)?
            }
            _ => return Err(CryptoError::InvalidArgument("Unknown mode.".to_string())),
        };
        let mut file = Vec::with_capacity(5 + payload.len());
        file.extend_from_slice(&Self::MAGIC);
        file.push(match mode {
            Self::MODE_TPM => 1,
            Self::MODE_PASSPHRASE => 2,
            _ => 3,
        });
        file.extend_from_slice(&payload);

        if let Some(parent) = self.key_file.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut tmp_os = self.key_file.as_os_str().to_os_string();
        tmp_os.push(".tmp");
        let tmp = PathBuf::from(tmp_os);
        {
            let mut f = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .share_mode(0)
                .open(&tmp)?;
            f.write_all(&file)?;
            f.sync_all()?;
        }
        win32::move_durable(&tmp, &self.key_file)?;
        Ok(())
    }

    fn wrap_tpm(&self, key: &SecureKey) -> Result<Vec<u8>, CryptoError> {
        let wrapped = key.use_key(|k| win32::tpm_encrypt_oaep_sha1(&self.tpm_key_name, k))?
            .map_err(|hr| {
                CryptoError::Cryptographic(format!("TPM encrypt failed: 0x{:08X}", hr as u32))
            })?;
        let name = self.tpm_key_name.as_bytes();
        let mut payload = Vec::with_capacity(2 + name.len() + 4 + wrapped.len());
        payload.extend_from_slice(&(name.len() as u16).to_le_bytes());
        payload.extend_from_slice(name);
        payload.extend_from_slice(&(wrapped.len() as i32).to_le_bytes());
        payload.extend_from_slice(&wrapped);
        Ok(payload)
    }
}

fn note_passphrase_locked(st: &mut VaultState, ok: bool) {
    if ok {
        st.pass_failures = 0;
        st.pass_locked_until_ms = 0;
        return;
    }
    st.pass_failures += 1;
    if st.pass_failures < UnlockPassword::FREE_TRIES {
        return;
    }
    let extra = (st.pass_failures - UnlockPassword::FREE_TRIES).min(5) as u32;
    let seconds = (30i64 * (1i64 << extra)).min(900);
    st.pass_locked_until_ms = ((st.now)() + ChronoDuration::seconds(seconds)).timestamp_millis();
    Log::info(&format!(
        "passphrase wrong ({} in a row), tries paused",
        st.pass_failures
    ));
}

fn read_file_mode(key_file: &Path) -> Result<Option<&'static str>, String> {
    if !key_file.exists() {
        return Ok(None);
    }
    let file = fs::read(key_file).map_err(|e| format!("IO: {:?}", e.kind()))?;
    if file.len() < 5 || file[..4] != KeyVault::MAGIC {
        return Err("Bad key file.".to_string());
    }
    let mode = match file[4] {
        1 => KeyVault::MODE_TPM,
        2 => KeyVault::MODE_PASSPHRASE,
        3 => KeyVault::MODE_DPAPI,
        _ => return Err("Bad key file mode.".to_string()),
    };
    if !KeyVault::payload_valid(&file, mode) {
        return Err("Truncated or damaged key file.".to_string());
    }
    Ok(Some(mode))
}

fn require_valid(file: &[u8], mode: &str) -> Result<(), CryptoError> {
    if !KeyVault::payload_valid(file, mode) {
        return Err(CryptoError::KeyLost("The key file is damaged.".to_string()));
    }
    Ok(())
}

fn checked_32(mut plain: Vec<u8>, iso: bool) -> Result<SecureKey, CryptoError> {
    let res = if plain.len() != 32 {
        Err(CryptoError::KeyLost("The key file is damaged.".to_string()))
    } else {
        SecureKey::new(&plain, iso)
    };
    plain.zeroize();
    res
}

fn dpapi_wrap(data: &[u8]) -> Result<Vec<u8>, CryptoError> {
    let blob = win32::dpapi_protect(data, "BetterRecall data key")
        .map_err(|code| CryptoError::Cryptographic(format!("CryptProtectData failed: {code}")))?;
    let mut payload = Vec::with_capacity(4 + blob.len());
    payload.extend_from_slice(&(blob.len() as i32).to_le_bytes());
    payload.extend_from_slice(&blob);
    Ok(payload)
}

fn unwrap_dpapi(file: &[u8], iso: bool) -> Result<SecureKey, CryptoError> {
    require_valid(file, KeyVault::MODE_DPAPI)?;
    let p = &file[5..];
    let len = i32::from_le_bytes(p[0..4].try_into().unwrap()) as usize;
    let plain = win32::dpapi_unprotect(&p[4..4 + len]).map_err(|_| {
        CryptoError::KeyLost(
            "DPAPI cannot decrypt the data key (another Windows account, or its keys were reset)."
                .to_string(),
        )
    })?;
    checked_32(plain, iso)
}

fn unwrap_tpm(file: &[u8], iso: bool) -> Result<SecureKey, CryptoError> {
    require_valid(file, KeyVault::MODE_TPM)?;
    let p = &file[5..];
    let name_len = u16::from_le_bytes([p[0], p[1]]) as usize;
    let name = std::str::from_utf8(&p[2..2 + name_len])
        .map_err(|_| CryptoError::KeyLost("The key file is damaged.".to_string()))?;
    let len = i32::from_le_bytes(p[2 + name_len..6 + name_len].try_into().unwrap()) as usize;
    let wrapped = &p[6 + name_len..6 + name_len + len];
    let plain = match win32::tpm_decrypt_oaep_sha1(name, wrapped) {
        Ok(v) => v,
        Err(hr) if hr == win32::NTE_BAD_KEYSET => {
            return Err(CryptoError::KeyLost(
                "The TPM key that wraps the data key no longer exists.".to_string(),
            ));
        }
        Err(hr) if !win32::tpm_transient(hr) => {
            return Err(CryptoError::KeyLost(
                "The TPM cannot unwrap the data key (damaged key file or a cleared TPM)."
                    .to_string(),
            ));
        }
        Err(hr) => {
            return Err(CryptoError::Cryptographic(format!(
                "TPM transient error: 0x{:08X}",
                hr as u32
            )));
        }
    };
    checked_32(plain, iso)
}

fn wrap_passphrase(key: &SecureKey, passphrase: &str) -> Result<Vec<u8>, CryptoError> {
    let mut salt = [0u8; 16];
    win32::fill_random(&mut salt);
    let kek = KeyVault::argon(
        passphrase,
        &salt,
        KeyVault::ARGON_MEMORY_KIB,
        KeyVault::ARGON_PASSES,
        KeyVault::ARGON_LANES,
    )?;
    let mut nonce = [0u8; 12];
    win32::fill_random(&mut nonce);
    let gcm = Aes256Gcm::new_from_slice(&kek)
        .map_err(|_| CryptoError::Cryptographic("Invalid KEK.".to_string()))?;

    let (ct, tag) = key.use_key(|k| {
        let mut buf = k.to_vec();
        let tag = gcm
            .encrypt_in_place_detached(Nonce::from_slice(&nonce), b"BRK1-pass", &mut buf)
            .map_err(|_| CryptoError::Cryptographic("Passphrase wrap failed.".to_string()))?;
        Ok::<_, CryptoError>((buf, tag))
    })??;

    let mut payload = Vec::with_capacity(KeyVault::PASSPHRASE_PAYLOAD);
    payload.extend_from_slice(&KeyVault::ARGON_MEMORY_KIB.to_le_bytes());
    payload.extend_from_slice(&KeyVault::ARGON_PASSES.to_le_bytes());
    payload.extend_from_slice(&KeyVault::ARGON_LANES.to_le_bytes());
    payload.extend_from_slice(&salt);
    payload.extend_from_slice(&nonce);
    payload.extend_from_slice(&ct);
    payload.extend_from_slice(tag.as_slice());
    Ok(payload)
}

fn unwrap_passphrase(
    file: &[u8],
    passphrase: &str,
    iso: bool,
) -> Result<Option<SecureKey>, CryptoError> {
    require_valid(file, KeyVault::MODE_PASSPHRASE)?;
    let p = &file[5..];
    let mem = i32::from_le_bytes(p[0..4].try_into().unwrap());
    let passes = i32::from_le_bytes(p[4..8].try_into().unwrap());
    let lanes = i32::from_le_bytes(p[8..12].try_into().unwrap());
    let salt = &p[12..28];
    let nonce = &p[28..40];
    let ct = &p[40..72];
    let tag = &p[72..88];

    let kek = KeyVault::argon(passphrase, salt, mem, passes, lanes)?;
    let gcm = Aes256Gcm::new_from_slice(&kek)
        .map_err(|_| CryptoError::Cryptographic("Invalid KEK.".to_string()))?;
    let mut plain = Zeroizing::new(ct.to_vec());
    if gcm
        .decrypt_in_place_detached(
            Nonce::from_slice(nonce),
            b"BRK1-pass",
            &mut plain,
            Tag::from_slice(tag),
        )
        .is_err()
    {
        return Ok(None);
    }
    Ok(Some(SecureKey::new(&plain, iso)?))
}
