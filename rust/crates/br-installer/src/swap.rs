// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Atomic-style folder swap with retry and rollback so an install is never half-deleted.

use std::fmt::Write as _;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use windows::Win32::Security::Cryptography::{BCRYPT_USE_SYSTEM_PREFERRED_RNG, BCryptGenRandom};

use crate::InstallError;
use crate::ctx::Ctx;
use crate::extract::Staged;
use crate::log::log_line;

/// Prefix for renamed-aside old install folders.
pub const OLD_PREFIX: &str = ".BetterRecall.old-";

const RENAME_RETRIES: usize = 10;
const RENAME_DELAY: Duration = Duration::from_millis(250);

/// Moves any existing install aside, renames `staged.dir` into `ctx.install_dir`,
/// and rolls back on failure.
pub fn swap_into_place(ctx: &Ctx, staged: Staged) -> Result<(), InstallError> {
    let parent = ctx
        .install_dir
        .parent()
        .ok_or_else(|| InstallError::Other("install_dir has no parent directory".to_string()))?;

    sweep_old_folders(parent, &ctx.home);

    let moved_old = if ctx.install_dir.exists() {
        let old_dir = parent.join(format!("{OLD_PREFIX}{}", random_hex_16()));
        if let Err(e) = rename_with_retry(&ctx.install_dir, &old_dir) {
            log_line(
                &ctx.home,
                &format!(
                    "swap: failed to rename {} aside: {e}",
                    ctx.install_dir.display()
                ),
            );
            remove_dir_logged(&staged.dir, &ctx.home, "staging folder");
            return Err(InstallError::InUse {
                path: ctx.install_dir.clone(),
            });
        }
        Some(old_dir)
    } else {
        None
    };

    if let Err(e) = rename_with_retry(&staged.dir, &ctx.install_dir) {
        log_line(
            &ctx.home,
            &format!(
                "swap: failed to move staging folder into {}: {e}",
                ctx.install_dir.display()
            ),
        );
        if let Some(old_dir) = &moved_old
            && let Err(rb_err) = rename_with_retry(old_dir, &ctx.install_dir)
        {
            log_line(
                &ctx.home,
                &format!(
                    "swap: rollback failed moving {} back to {}: {rb_err}",
                    old_dir.display(),
                    ctx.install_dir.display()
                ),
            );
        }
        remove_dir_logged(&staged.dir, &ctx.home, "staging folder");
        return Err(InstallError::io(
            format!("moving staged install into {}", ctx.install_dir.display()),
            e,
        ));
    }

    if let Some(old_dir) = moved_old {
        remove_dir_logged(&old_dir, &ctx.home, "old install folder");
    }

    Ok(())
}

/// Renames `from` to `to`, retrying 10 times with 250 ms between attempts (~2.25 s total).
pub(crate) fn rename_with_retry(from: &Path, to: &Path) -> io::Result<()> {
    for attempt in 0..RENAME_RETRIES {
        match fs::rename(from, to) {
            Ok(()) => return Ok(()),
            Err(_) if attempt + 1 < RENAME_RETRIES => thread::sleep(RENAME_DELAY),
            Err(e) => return Err(e),
        }
    }
    unreachable!("RENAME_RETRIES > 0")
}

/// Generates a unique `.BetterRecall.old-<16 hex>` sibling path under `parent`.
pub(crate) fn old_folder_path(parent: &Path) -> PathBuf {
    parent.join(format!("{OLD_PREFIX}{}", random_hex_16()))
}

/// Best-effort sweep of leftover `.BetterRecall.old-*` folders beside `install_dir`.
pub(crate) fn sweep_old_folders(parent: &Path, home: &Path) {
    let Ok(entries) = fs::read_dir(parent) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name_str) = name.to_str() else {
            continue;
        };
        if !name_str.starts_with(OLD_PREFIX) {
            continue;
        }
        remove_dir_logged(&entry.path(), home, "leftover old install folder");
    }
}

fn remove_dir_logged(dir: &Path, home: &Path, label: &str) {
    if !dir.exists() {
        return;
    }
    if let Err(e) = fs::remove_dir_all(dir) {
        log_line(
            home,
            &format!("cleanup: failed to remove {label} {}: {e}", dir.display()),
        );
    }
}

fn random_hex_16() -> String {
    let mut bytes = [0u8; 8];
    let status = unsafe { BCryptGenRandom(None, &mut bytes, BCRYPT_USE_SYSTEM_PREFERRED_RNG) };
    assert!(status.is_ok(), "BCryptGenRandom failed");
    let mut out = String::with_capacity(16);
    for b in bytes {
        let _ = write!(&mut out, "{b:02x}");
    }
    out
}
