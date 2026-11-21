// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Dynamic FFI bindings for `avif.dll`, `jxl.dll` + `jxl_threads.dll`, and `libSkiaSharp.dll`.

pub mod avif;
pub mod jxl;
pub mod skia;

use std::path::{Path, PathBuf};

use libloading::Library;
use libloading::os::windows::Library as WinLibrary;
use windows::Win32::System::LibraryLoader::{
    LOAD_LIBRARY_SEARCH_DEFAULT_DIRS, LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR,
};

pub use avif::Avif;
pub use jxl::Jxl;
pub use skia::Skia;

/// Locates a native codec DLL bundled under `src\BetterRecall.Agent\native\` (`avif.dll`,
/// `jxl.dll`, `jxl_threads.dll`).
///
/// Release builds load only from the executable's own folder; debug builds (except when the exe
/// is staged as `BetterRecall.Agent.exe`) may also fall back to `BR_NATIVE_DIR` or the repo's
/// `src\BetterRecall.Agent\native\` folder.
pub fn find_agent_native_dll(file_name: &str) -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let exe_dir = exe.parent()?;
    let local = exe_dir.join(file_name);
    if local.is_file() {
        return Some(local);
    }

    #[cfg(not(debug_assertions))]
    {
        None
    }

    #[cfg(debug_assertions)]
    {
        if exe
            .file_name()
            .is_some_and(|n| n.eq_ignore_ascii_case("BetterRecall.Agent.exe"))
        {
            return None;
        }
        if let Some(dir) = std::env::var_os("BR_NATIVE_DIR") {
            let p = PathBuf::from(dir).join(file_name);
            if p.is_file() {
                return Some(p);
            }
        }
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        // rust/crates/br-agent -> repo root -> src/BetterRecall.Agent/native
        if let Some(repo_root) = manifest_dir.ancestors().nth(3) {
            let repo_native = repo_root
                .join("src")
                .join("BetterRecall.Agent")
                .join("native")
                .join(file_name);
            if repo_native.is_file() {
                return Some(repo_native);
            }
        }
        None
    }
}

/// Locates `libSkiaSharp.dll`.
///
/// Release builds load only from the executable's own folder; debug builds (except when the exe
/// is staged as `BetterRecall.Agent.exe`) may also fall back to `BR_NATIVE_DIR` or the NuGet
/// package cache (`%USERPROFILE%\.nuget\packages\skiasharp.nativeassets.win32\3.119.1\runtimes\win-x64\native\`).
pub fn find_skia_dll() -> Option<PathBuf> {
    const FILE_NAME: &str = "libSkiaSharp.dll";
    let exe = std::env::current_exe().ok()?;
    let exe_dir = exe.parent()?;
    let local = exe_dir.join(FILE_NAME);
    if local.is_file() {
        return Some(local);
    }

    #[cfg(not(debug_assertions))]
    {
        None
    }

    #[cfg(debug_assertions)]
    {
        if exe
            .file_name()
            .is_some_and(|n| n.eq_ignore_ascii_case("BetterRecall.Agent.exe"))
        {
            return None;
        }
        if let Some(dir) = std::env::var_os("BR_NATIVE_DIR") {
            let p = PathBuf::from(dir).join(FILE_NAME);
            if p.is_file() {
                return Some(p);
            }
        }
        if let Some(profile) = std::env::var_os("USERPROFILE") {
            let pkg_root = PathBuf::from(profile)
                .join(".nuget")
                .join("packages")
                .join("skiasharp.nativeassets.win32");
            let pinned = pkg_root
                .join("3.119.1")
                .join("runtimes")
                .join("win-x64")
                .join("native")
                .join(FILE_NAME);
            if pinned.is_file() {
                return Some(pinned);
            }
            if let Ok(entries) = std::fs::read_dir(&pkg_root) {
                let mut dirs: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
                dirs.sort();
                for ver in dirs.into_iter().rev() {
                    let cand = ver
                        .join("runtimes")
                        .join("win-x64")
                        .join("native")
                        .join(FILE_NAME);
                    if cand.is_file() {
                        return Some(cand);
                    }
                }
            }
        }
        None
    }
}

pub(crate) fn load_native_library(path: &Path) -> Result<Library, libloading::Error> {
    let flags = LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR.0 | LOAD_LIBRARY_SEARCH_DEFAULT_DIRS.0;
    // SAFETY: Loading a native DLL with explicit search flags (DLL load dir + default dirs).
    let win_lib = unsafe { WinLibrary::load_with_flags(path, flags) }?;
    Ok(win_lib.into())
}

pub(crate) fn resolve_sym<T: Copy>(lib: &Library, name: &[u8]) -> Result<T, String> {
    // SAFETY: Caller supplies the exact C function pointer type for the exported symbol.
    unsafe {
        lib.get::<T>(name)
            .map(|s| *s)
            .map_err(|e| format!("missing export {}: {e}", String::from_utf8_lossy(name)))
    }
}
