// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

use std::env;
use std::ffi::{OsString, c_void};
use std::io;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use windows::Win32::Foundation::{CloseHandle, GetLastError, HANDLE, HLOCAL, LocalFree};
use windows::Win32::Security::Authorization::ConvertSidToStringSidW;
use windows::Win32::Security::Cryptography::{BCRYPT_USE_SYSTEM_PREFERRED_RNG, BCryptGenRandom};
use windows::Win32::Security::{GetTokenInformation, TOKEN_QUERY, TOKEN_USER, TokenUser};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_NAME_NORMALIZED, FILE_SHARE_DELETE,
    FILE_SHARE_READ, FILE_SHARE_WRITE, GetFinalPathNameByHandleW, GetFullPathNameW, OPEN_EXISTING,
};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows::Win32::UI::Shell::{FOLDERID_LocalAppData, KF_FLAG_DEFAULT, SHGetKnownFolderPath};
use windows::core::{PCWSTR, PWSTR};

use crate::settings::Settings;

/// Resolves a Windows path lexically using `GetFullPathNameW` (matching .NET `Path.GetFullPath`).
pub fn full_path(path: &Path) -> io::Result<PathBuf> {
    if path.as_os_str().is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "path is empty",
        ));
    }
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    if wide[..wide.len() - 1].contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "path contains null byte",
        ));
    }
    let mut buf = vec![0u16; 1024];
    loop {
        let len = unsafe { GetFullPathNameW(PCWSTR(wide.as_ptr()), Some(&mut buf), None) };
        if len == 0 {
            return Err(io::Error::last_os_error());
        }
        if (len as usize) < buf.len() {
            return Ok(PathBuf::from(OsString::from_wide(&buf[..len as usize])));
        }
        buf.resize(len as usize + 1, 0);
    }
}

/// True when `p` is a fully qualified Windows path (matching .NET `Path.IsPathFullyQualified`).
pub(crate) fn is_path_fully_qualified(p: &str) -> bool {
    let b = p.as_bytes();
    if b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b[2] == b'\\' || b[2] == b'/')
    {
        return true;
    }
    b.len() >= 2 && ((b[0] == b'\\' && b[1] == b'\\') || (b[0] == b'/' && b[1] == b'/'))
}

fn local_app_data() -> PathBuf {
    unsafe {
        if let Ok(pwstr) = SHGetKnownFolderPath(&FOLDERID_LocalAppData, KF_FLAG_DEFAULT, None) {
            let mut len = 0usize;
            while *pwstr.0.add(len) != 0 {
                len += 1;
            }
            let slice = std::slice::from_raw_parts(pwstr.0, len);
            let s = OsString::from_wide(slice);
            CoTaskMemFree(Some(pwstr.0.cast()));
            return PathBuf::from(s);
        }
    }
    env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_default()
}

struct OwnedHandle(HANDLE);
impl Drop for OwnedHandle {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }
}

struct LocalMem(*mut c_void);
impl Drop for LocalMem {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                let _ = LocalFree(Some(HLOCAL(self.0)));
            }
        }
    }
}

/// Exe paths relative to the install dir. Trust checks compare full image paths against these.
pub struct InstallLayout;

impl InstallLayout {
    pub const AGENT_EXE: &'static str = "BetterRecall.Agent.exe";
    pub const CLI_EXE: &'static str = "betterrecall.exe";
    pub const MCP_EXE: &'static str = "betterrecall-mcp.exe";
    pub const WORKER_EXE: &'static str = r"worker\BetterRecall.Worker.exe";
    pub const UI_EXE: &'static str = r"ui\BetterRecall.UI.exe";

    pub const CLI_SHIM: &'static str = "betterrecall.cmd";

    pub const MCP_RELAY_ENV: &'static str = "BETTERRECALL_MCP_RELAY";
    pub const MCP_RESTART_EXIT_CODE: i32 = 75;

    pub const DEV_BUILD_MARKER: &'static str = ".devbuild";
    pub const INSTALL_MARKER: &'static str = ".install";
    pub const ALLOW_DEFAULT_HOME_FLAG: &'static str = "--allow-default-home";

    /// Default per-user install folder: `%LOCALAPPDATA%\Programs\BetterRecall`.
    pub fn default_install_dir() -> PathBuf {
        local_app_data().join("Programs").join("BetterRecall")
    }

    pub fn full(install_dir: &Path, relative: &str) -> PathBuf {
        let joined = install_dir.join(relative);
        full_path(&joined).unwrap_or(joined)
    }

    /// A launcher install folder name: `<version>-<hash12>` (12 lower-case hex digits).
    pub fn is_version_folder(name: &str) -> bool {
        let Some(dash) = name.rfind('-') else {
            return false;
        };
        if dash == 0 || name.len() - dash - 1 != 12 {
            return false;
        }
        if !name.as_bytes()[0].is_ascii_digit() {
            return false;
        }
        name[dash + 1..]
            .bytes()
            .all(|b| b.is_ascii_digit() || matches!(b, b'a'..=b'f'))
    }

    /// Resolves SUBST drives, junctions and symlinks via `GetFinalPathNameByHandleW`; for a path
    /// not created yet, resolves its nearest existing ancestor and appends the rest.
    pub fn real_path(path: &Path) -> PathBuf {
        if path.to_string_lossy().trim().is_empty() {
            return path.to_path_buf();
        }
        let Ok(mut full) = full_path(path) else {
            return path.to_path_buf();
        };
        let full_str = full.to_string_lossy();
        if full_str.len() > 3 && full_str.ends_with('\\') {
            full = PathBuf::from(full_str.trim_end_matches('\\'));
        }
        let mut rest: Vec<OsString> = Vec::new();
        let mut cur = Some(full.as_path());
        while let Some(p) = cur {
            if p.exists()
                && let Some(mut real) = final_path(p)
            {
                for part in rest.into_iter().rev() {
                    real.push(part);
                }
                return real;
            }
            if let Some(leaf) = p.file_name()
                && !leaf.is_empty()
            {
                rest.push(leaf.to_os_string());
                cur = p.parent();
            } else {
                break;
            }
        }
        full
    }

    /// True when a dev build (`.devbuild` marker present) would open the default data folder
    /// without `--allow-default-home` and without the installer's `.install` marker.
    pub fn refuses_home(install_dir: &Path, home: &Path, allow_default_home: bool) -> bool {
        if allow_default_home
            || !DataPaths::is_default_home(home)
            || !install_dir.join(Self::DEV_BUILD_MARKER).exists()
        {
            return false;
        }
        !install_dir.join(Self::INSTALL_MARKER).exists()
    }
}

fn final_path(path: &Path) -> Option<PathBuf> {
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let h = unsafe {
        CreateFileW(
            PCWSTR(wide.as_ptr()),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            None,
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            None,
        )
    }
    .ok()?;
    let _guard = OwnedHandle(h);
    let mut buf = vec![0u16; 1024];
    let mut n = unsafe { GetFinalPathNameByHandleW(h, &mut buf, FILE_NAME_NORMALIZED) };
    if n == 0 {
        return None;
    }
    if n as usize >= buf.len() {
        buf.resize(n as usize + 1, 0);
        n = unsafe { GetFinalPathNameByHandleW(h, &mut buf, FILE_NAME_NORMALIZED) };
        if n == 0 || n as usize >= buf.len() {
            return None;
        }
    }
    let s = OsString::from_wide(&buf[..n as usize])
        .to_string_lossy()
        .into_owned();
    let cleaned = if s.len() >= 8 && s[..8].eq_ignore_ascii_case(r"\\?\UNC\") {
        format!(r"\\{}", &s[8..])
    } else if let Some(stripped) = s.strip_prefix(r"\\?\") {
        stripped.to_string()
    } else {
        s
    };
    if cleaned.len() > 3 {
        Some(PathBuf::from(cleaned.trim_end_matches('\\')))
    } else {
        Some(PathBuf::from(cleaned))
    }
}

/// Data dir layout. `BETTERRECALL_HOME` overrides `%LOCALAPPDATA%\BetterRecall`.
pub struct DataPaths;

impl DataPaths {
    pub const HOME_ENV: &'static str = "BETTERRECALL_HOME";

    pub fn home() -> PathBuf {
        if let Some(env_val) = env::var_os(Self::HOME_ENV) {
            let s = env_val.to_string_lossy();
            if !s.trim().is_empty() {
                let p = PathBuf::from(env_val);
                return full_path(&p).unwrap_or(p);
            }
        }
        local_app_data().join("BetterRecall")
    }

    pub fn is_default_home(home: &Path) -> bool {
        let normalized = full_path(home).unwrap_or_else(|_| home.to_path_buf());
        let trimmed = normalized
            .to_string_lossy()
            .trim_end_matches(['\\', '/'])
            .to_string();
        let default_home = local_app_data().join("BetterRecall");
        trimmed.eq_ignore_ascii_case(&default_home.to_string_lossy())
    }

    pub fn settings_file(home: &Path) -> PathBuf {
        home.join("settings.json")
    }

    pub fn store_db(home: &Path) -> PathBuf {
        home.join("store.db")
    }

    pub fn keyframes_dir(home: &Path) -> PathBuf {
        home.join("keyframes")
    }

    pub fn replays_dir(home: &Path) -> PathBuf {
        home.join("replays")
    }

    pub fn key_file(home: &Path) -> PathBuf {
        home.join("keys").join("datakey.bin")
    }

    pub fn unlock_password_file(home: &Path) -> PathBuf {
        home.join("keys").join("unlock.bin")
    }

    pub fn ai_key_file(home: &Path) -> PathBuf {
        home.join("keys").join("ai-key.bin")
    }

    pub fn models_dir(home: &Path, s: &Settings) -> PathBuf {
        if s.inference.model_dir.trim().is_empty() {
            home.join("models")
        } else {
            PathBuf::from(&s.inference.model_dir)
        }
    }

    pub fn compiled_cache(home: &Path) -> PathBuf {
        home.join("cache").join("compiled")
    }

    pub fn device_cache(home: &Path) -> PathBuf {
        home.join("cache").join("device.json")
    }

    pub fn bench_report(home: &Path) -> PathBuf {
        home.join("cache").join("bench.json")
    }

    pub fn updates_dir(home: &Path) -> PathBuf {
        home.join("cache").join("updates")
    }

    pub fn logs_dir(home: &Path) -> PathBuf {
        home.join("logs")
    }

    pub fn agent_pid_file(home: &Path) -> PathBuf {
        home.join("agent.pid")
    }

    pub fn mcp_relay(home: &Path) -> PathBuf {
        home.join("app").join("bin").join(InstallLayout::MCP_EXE)
    }
}

/// Pipe and single-instance naming helpers.
pub struct PipeNames;

impl PipeNames {
    pub const AGENT_MUTEX: &'static str = r"Local\BetterRecall.Agent";
    pub const SETUP_MUTEX: &'static str = r"Local\BetterRecall.Setup";
    // B1: scoped setup lock for installer-vs-installer exclusion.
    pub const SETUP_LOCK: &'static str = r"Local\BetterRecall.SetupLock";
    pub const SHOW_LAUNCHER_EVENT: &'static str = r"Local\BetterRecall.UI.ShowLauncher";
    pub const UI_INSTANCE_KEY: &'static str = "BetterRecall.UI";
    pub const AGENT_QUIT_EVENT: &'static str = r"Local\BetterRecall.Agent.Quit";
    pub const UI_QUIT_EVENT: &'static str = r"Local\BetterRecall.UI.Quit";

    /// Current Windows user SID string (e.g. `S-1-5-21-...`).
    pub fn user_sid() -> io::Result<String> {
        unsafe {
            let mut token = HANDLE::default();
            OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token)
                .map_err(|e| io::Error::from_raw_os_error(e.code().0))?;
            let _token_guard = OwnedHandle(token);

            let mut needed = 0u32;
            let _ = GetTokenInformation(token, TokenUser, None, 0, &mut needed);
            if needed == 0 {
                return Err(io::Error::from_raw_os_error(GetLastError().0 as i32));
            }
            let mut buf = vec![0u8; needed as usize];
            GetTokenInformation(
                token,
                TokenUser,
                Some(buf.as_mut_ptr().cast::<c_void>()),
                needed,
                &mut needed,
            )
            .map_err(|e| io::Error::from_raw_os_error(e.code().0))?;

            let user = &*(buf.as_ptr().cast::<TOKEN_USER>());
            let mut sid_pwstr = PWSTR::null();
            ConvertSidToStringSidW(user.User.Sid, &mut sid_pwstr)
                .map_err(|e| io::Error::from_raw_os_error(e.code().0))?;
            let _sid_guard = LocalMem(sid_pwstr.0.cast::<c_void>());
            let mut len = 0usize;
            while *sid_pwstr.0.add(len) != 0 {
                len += 1;
            }
            let slice = std::slice::from_raw_parts(sid_pwstr.0, len);
            Ok(OsString::from_wide(slice).to_string_lossy().into_owned())
        }
    }

    /// Strips `\\.\pipe\` or `\pipe\` prefixes (case-insensitive).
    pub fn base(configured: &str) -> &str {
        for prefix in [r"\\.\pipe\", r"\pipe\"] {
            if configured.len() >= prefix.len()
                && configured.is_char_boundary(prefix.len())
                && configured[..prefix.len()].eq_ignore_ascii_case(prefix)
            {
                return &configured[prefix.len()..];
            }
        }
        configured
    }

    /// Base names a configured public pipe may not take: they belong to the UI, MCP and worker pipes.
    pub fn reserved(configured_base: &str) -> bool {
        let b = Self::base(configured_base);
        ["BetterRecall.ui", "BetterRecall.mcp", "BetterRecall.worker"]
            .iter()
            .any(|r| {
                b.len() >= r.len()
                    && b.is_char_boundary(r.len())
                    && b[..r.len()].eq_ignore_ascii_case(r)
            })
    }

    pub fn public(configured_base: &str, sid: &str, home: &Path) -> String {
        Self::scoped(&format!("{}.{sid}", Self::base(configured_base)), home)
    }

    pub fn mcp(sid: &str, home: &Path) -> String {
        Self::scoped(&format!("BetterRecall.mcp.{sid}"), home)
    }

    pub fn ui(sid: &str, home: &Path) -> String {
        Self::scoped(&format!("BetterRecall.ui.{sid}"), home)
    }

    /// Generates a new private worker pipe name `BetterRecall.worker.<32 hex>`.
    pub fn new_worker() -> String {
        let mut bytes = [0u8; 16];
        let status = unsafe { BCryptGenRandom(None, &mut bytes, BCRYPT_USE_SYSTEM_PREFERRED_RNG) };
        assert!(status.is_ok(), "BCryptGenRandom failed");
        let mut s = String::with_capacity("BetterRecall.worker.".len() + 32);
        s.push_str("BetterRecall.worker.");
        for b in bytes {
            use std::fmt::Write;
            let _ = write!(&mut s, "{b:02x}");
        }
        s
    }

    pub fn agent_mutex_for(home: &Path) -> String {
        Self::scoped(Self::AGENT_MUTEX, home)
    }

    pub fn setup_mutex_for(home: &Path) -> String {
        Self::scoped(Self::SETUP_MUTEX, home)
    }

    // B1: scoped setup lock mirroring setup_mutex_for.
    pub fn setup_lock_for(home: &Path) -> String {
        Self::scoped(Self::SETUP_LOCK, home)
    }

    pub fn agent_quit_event_for(home: &Path) -> String {
        Self::scoped(Self::AGENT_QUIT_EVENT, home)
    }

    pub fn show_launcher_event_for(home: &Path) -> String {
        Self::scoped(Self::SHOW_LAUNCHER_EVENT, home)
    }

    pub fn ui_instance_key_for(home: &Path) -> String {
        Self::scoped(Self::UI_INSTANCE_KEY, home)
    }

    pub fn ui_quit_event_for(home: &Path) -> String {
        Self::scoped(Self::UI_QUIT_EVENT, home)
    }

    /// `name` for the default data folder, else `name + "." + 16 uppercase hex of SHA-256 over the full upper-case path`.
    pub fn scoped(name: &str, home: &Path) -> String {
        if DataPaths::is_default_home(home) {
            return name.to_string();
        }
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
        format!("{name}.{hex}")
    }
}
