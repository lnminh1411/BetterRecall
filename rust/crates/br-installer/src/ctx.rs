// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Installer target locations (`install_dir`, `home`, HKCU registry prefix, Start menu folder).

use std::env;
use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;
use std::path::{Path, PathBuf};

use br_contracts::{DataPaths, full_path};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::UI::Shell::{
    FOLDERID_LocalAppData, FOLDERID_Programs, FOLDERID_UserProgramFiles, KF_FLAG_DEFAULT,
    SHGetKnownFolderPath,
};
use windows::core::GUID;

use crate::InstallError;

/// Required HKCU prefix for test and `--dev-reg-prefix` runs so they never touch real keys.
pub const TEST_REG_PREFIX: &str = r"Software\BetterRecallTests\";

/// Every location the installer reads or writes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ctx {
    pub install_dir: PathBuf,
    pub home: PathBuf,
    pub reg_prefix: String,
    pub start_menu_dir: PathBuf,
    pub temp_dir: PathBuf,
    pub start_after: bool,
}

impl Ctx {
    /// Production per-user install context.
    pub fn production() -> Self {
        let programs = known_folder(&FOLDERID_UserProgramFiles)
            .unwrap_or_else(|| local_app_data().join("Programs"));
        let start_menu_dir = known_folder(&FOLDERID_Programs).unwrap_or_else(|| {
            env::var_os("APPDATA")
                .map(PathBuf::from)
                .unwrap_or_else(local_app_data)
                .join(r"Microsoft\Windows\Start Menu\Programs")
        });
        Self {
            install_dir: programs.join("BetterRecall"),
            home: DataPaths::home(),
            reg_prefix: String::new(),
            start_menu_dir,
            temp_dir: env::temp_dir(),
            start_after: true,
        }
    }

    /// Scratch context for tests and `--dev-root` / `--dev-reg-prefix`.
    /// Rejects any `reg_prefix` outside `Software\BetterRecallTests\` and rejects the default home.
    pub fn from_dev_root(root: &Path, reg_prefix: &str) -> Result<Self, InstallError> {
        let trimmed_prefix = reg_prefix.trim_end_matches('\\');
        if !reg_prefix.starts_with(TEST_REG_PREFIX) || trimmed_prefix.len() < TEST_REG_PREFIX.len()
        {
            return Err(InstallError::Usage(format!(
                "test/dev registry prefix must be under {TEST_REG_PREFIX} (got '{reg_prefix}')"
            )));
        }
        let root_full = full_path(root).unwrap_or_else(|_| root.to_path_buf());
        let home = root_full.join("home");
        if DataPaths::is_default_home(&root_full) || DataPaths::is_default_home(&home) {
            return Err(InstallError::Usage(
                "test/dev root must not target the default BetterRecall data folder".to_string(),
            ));
        }
        Ok(Self {
            install_dir: root_full.join("install"),
            home,
            reg_prefix: trimmed_prefix.to_string(),
            start_menu_dir: root_full.join("start-menu"),
            temp_dir: root_full.join("temp"),
            start_after: false,
        })
    }

    /// Scratch context for unit/integration tests. Panics if `reg_prefix` is outside
    /// `Software\BetterRecallTests\` or if `root`/`home` resolves to the default home.
    pub fn for_test(root: &Path, reg_prefix: &str) -> Self {
        Self::from_dev_root(root, reg_prefix)
            .unwrap_or_else(|e| panic!("invalid Ctx::for_test parameters: {e}"))
    }

    /// Prepends `self.reg_prefix` (if non-empty) to a relative HKCU subkey path.
    pub fn reg_key(&self, subkey: &str) -> String {
        let clean_sub = subkey.trim_start_matches('\\');
        if self.reg_prefix.is_empty() {
            clean_sub.to_string()
        } else {
            format!("{}\\{clean_sub}", self.reg_prefix.trim_end_matches('\\'))
        }
    }
}

fn known_folder(id: &GUID) -> Option<PathBuf> {
    unsafe {
        let pwstr = SHGetKnownFolderPath(id, KF_FLAG_DEFAULT, None).ok()?;
        let mut len = 0usize;
        while *pwstr.0.add(len) != 0 {
            len += 1;
        }
        let slice = std::slice::from_raw_parts(pwstr.0, len);
        let s = OsString::from_wide(slice);
        CoTaskMemFree(Some(pwstr.0.cast()));
        Some(PathBuf::from(s))
    }
}

fn local_app_data() -> PathBuf {
    known_folder(&FOLDERID_LocalAppData)
        .or_else(|| env::var_os("LOCALAPPDATA").map(PathBuf::from))
        .unwrap_or_default()
}
