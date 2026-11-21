// SPDX-License-Identifier: MIT
// Copyright (c) BetterRecall contributors

use std::env;
use std::ffi::OsString;
use std::io;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

use windows::Win32::Security::Cryptography::{BCRYPT_USE_SYSTEM_PREFERRED_RNG, BCryptGenRandom};
use windows::Win32::Storage::FileSystem::GetFullPathNameW;
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::UI::Shell::{FOLDERID_LocalAppData, KF_FLAG_DEFAULT, SHGetKnownFolderPath};
use windows::core::PCWSTR;

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

fn local_app_data() -> PathBuf {
    unsafe {
        if let Ok(pwstr) = SHGetKnownFolderPath(&FOLDERID_LocalAppData, KF_FLAG_DEFAULT, None)
        {
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

/// Exe paths relative to the install dir. Trust checks compare full image paths against these.
pub struct InstallLayout;

impl InstallLayout {
    pub const AGENT_EXE: &'static str = "BetterRecall.Agent.exe";
    pub const WORKER_EXE: &'static str = r"worker\BetterRecall.Worker.exe";

    pub fn full(install_dir: &Path, relative: &str) -> PathBuf {
        let joined = install_dir.join(relative);
        full_path(&joined).unwrap_or(joined)
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

    pub fn logs_dir(home: &Path) -> PathBuf {
        home.join("logs")
    }
}

/// Pipe naming helpers.
pub struct PipeNames;

impl PipeNames {
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

    /// Generates a new private worker pipe name `BetterRecall.worker.<32 hex>`.
    pub fn new_worker() -> String {
        let mut bytes = [0u8; 16];
        let status = unsafe {
            BCryptGenRandom(
                None,
                &mut bytes,
                BCRYPT_USE_SYSTEM_PREFERRED_RNG,
            )
        };
        assert!(status.is_ok(), "BCryptGenRandom failed");
        let mut s = String::with_capacity("BetterRecall.worker.".len() + 32);
        s.push_str("BetterRecall.worker.");
        for b in bytes {
            use std::fmt::Write;
            let _ = write!(&mut s, "{b:02x}");
        }
        s
    }
}
