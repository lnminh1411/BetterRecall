// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Raw dynamic FFI bindings to `sqlite3mc.dll` (SQLite3MultipleCiphers).

use std::ffi::{c_char, c_int, c_void};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use libloading::Library;
use libloading::os::windows::Library as WinLibrary;
use windows::Win32::System::LibraryLoader::{
    LOAD_LIBRARY_SEARCH_DEFAULT_DIRS, LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR,
};

pub const SQLITE_OK: c_int = 0;
pub const SQLITE_BUSY: c_int = 5;
pub const SQLITE_NOTADB: c_int = 26;
pub const SQLITE_ROW: c_int = 100;
pub const SQLITE_DONE: c_int = 101;

pub const SQLITE_OPEN_READONLY: c_int = 0x0000_0001;
pub const SQLITE_OPEN_READWRITE: c_int = 0x0000_0002;
pub const SQLITE_OPEN_CREATE: c_int = 0x0000_0004;
pub const SQLITE_OPEN_FULLMUTEX: c_int = 0x0001_0000;

pub const SQLITE_INTEGER: c_int = 1;
pub const SQLITE_FLOAT: c_int = 2;
pub const SQLITE_TEXT: c_int = 3;
pub const SQLITE_BLOB: c_int = 4;
pub const SQLITE_NULL: c_int = 5;

#[repr(C)]
pub struct Sqlite3 {
    _private: [u8; 0],
}

#[repr(C)]
pub struct Sqlite3Stmt {
    _private: [u8; 0],
}

pub type SqliteDestructor = Option<unsafe extern "C" fn(*mut c_void)>;
pub type SqliteExecCallback = Option<
    unsafe extern "C" fn(*mut c_void, c_int, *mut *mut c_char, *mut *mut c_char) -> c_int,
>;

/// `SQLITE_TRANSIENT` tells SQLite to make its own private copy of bound text/blob bytes immediately.
#[inline]
pub fn sqlite_transient() -> SqliteDestructor {
    // SAFETY: SQLite defines SQLITE_TRANSIENT as ((sqlite3_destructor_type)-1).
    unsafe { std::mem::transmute::<isize, SqliteDestructor>(-1isize) }
}

pub type FnOpenV2 =
    unsafe extern "C" fn(*const c_char, *mut *mut Sqlite3, c_int, *const c_char) -> c_int;
pub type FnCloseV2 = unsafe extern "C" fn(*mut Sqlite3) -> c_int;
pub type FnPrepareV2 = unsafe extern "C" fn(
    *mut Sqlite3,
    *const c_char,
    c_int,
    *mut *mut Sqlite3Stmt,
    *mut *const c_char,
) -> c_int;
pub type FnStep = unsafe extern "C" fn(*mut Sqlite3Stmt) -> c_int;
pub type FnReset = unsafe extern "C" fn(*mut Sqlite3Stmt) -> c_int;
pub type FnClearBindings = unsafe extern "C" fn(*mut Sqlite3Stmt) -> c_int;
pub type FnFinalize = unsafe extern "C" fn(*mut Sqlite3Stmt) -> c_int;
pub type FnBindNull = unsafe extern "C" fn(*mut Sqlite3Stmt, c_int) -> c_int;
pub type FnBindInt = unsafe extern "C" fn(*mut Sqlite3Stmt, c_int, c_int) -> c_int;
pub type FnBindInt64 = unsafe extern "C" fn(*mut Sqlite3Stmt, c_int, i64) -> c_int;
pub type FnBindDouble = unsafe extern "C" fn(*mut Sqlite3Stmt, c_int, f64) -> c_int;
pub type FnBindText = unsafe extern "C" fn(
    *mut Sqlite3Stmt,
    c_int,
    *const c_char,
    c_int,
    SqliteDestructor,
) -> c_int;
pub type FnBindBlob = unsafe extern "C" fn(
    *mut Sqlite3Stmt,
    c_int,
    *const c_void,
    c_int,
    SqliteDestructor,
) -> c_int;
pub type FnBindParameterIndex = unsafe extern "C" fn(*mut Sqlite3Stmt, *const c_char) -> c_int;
pub type FnColumnType = unsafe extern "C" fn(*mut Sqlite3Stmt, c_int) -> c_int;
pub type FnColumnInt = unsafe extern "C" fn(*mut Sqlite3Stmt, c_int) -> c_int;
pub type FnColumnInt64 = unsafe extern "C" fn(*mut Sqlite3Stmt, c_int) -> i64;
pub type FnColumnDouble = unsafe extern "C" fn(*mut Sqlite3Stmt, c_int) -> f64;
pub type FnColumnText = unsafe extern "C" fn(*mut Sqlite3Stmt, c_int) -> *const u8;
pub type FnColumnBlob = unsafe extern "C" fn(*mut Sqlite3Stmt, c_int) -> *const c_void;
pub type FnColumnBytes = unsafe extern "C" fn(*mut Sqlite3Stmt, c_int) -> c_int;
pub type FnChanges = unsafe extern "C" fn(*mut Sqlite3) -> c_int;
pub type FnLastInsertRowid = unsafe extern "C" fn(*mut Sqlite3) -> i64;
pub type FnErrmsg = unsafe extern "C" fn(*mut Sqlite3) -> *const c_char;
pub type FnErrcode = unsafe extern "C" fn(*mut Sqlite3) -> c_int;
pub type FnExtendedErrcode = unsafe extern "C" fn(*mut Sqlite3) -> c_int;
pub type FnBusyTimeout = unsafe extern "C" fn(*mut Sqlite3, c_int) -> c_int;
pub type FnExec = unsafe extern "C" fn(
    *mut Sqlite3,
    *const c_char,
    SqliteExecCallback,
    *mut c_void,
    *mut *mut c_char,
) -> c_int;
pub type FnFree = unsafe extern "C" fn(*mut c_void);
pub type FnGetAutocommit = unsafe extern "C" fn(*mut Sqlite3) -> c_int;

pub struct SqliteApi {
    _lib: Library,
    pub open_v2: FnOpenV2,
    pub close_v2: FnCloseV2,
    pub prepare_v2: FnPrepareV2,
    pub step: FnStep,
    pub reset: FnReset,
    pub clear_bindings: FnClearBindings,
    pub finalize: FnFinalize,
    pub bind_null: FnBindNull,
    pub bind_int: FnBindInt,
    pub bind_int64: FnBindInt64,
    pub bind_double: FnBindDouble,
    pub bind_text: FnBindText,
    pub bind_blob: FnBindBlob,
    pub bind_parameter_index: FnBindParameterIndex,
    pub column_type: FnColumnType,
    pub column_int: FnColumnInt,
    pub column_int64: FnColumnInt64,
    pub column_double: FnColumnDouble,
    pub column_text: FnColumnText,
    pub column_blob: FnColumnBlob,
    pub column_bytes: FnColumnBytes,
    pub changes: FnChanges,
    pub last_insert_rowid: FnLastInsertRowid,
    pub errmsg: FnErrmsg,
    pub errcode: FnErrcode,
    pub extended_errcode: FnExtendedErrcode,
    pub busy_timeout: FnBusyTimeout,
    pub exec: FnExec,
    pub free: FnFree,
    pub get_autocommit: FnGetAutocommit,
}

// SAFETY: `Library` holds an OS module handle (`HMODULE`) and all fields are immutable function pointers.
unsafe impl Send for SqliteApi {}
unsafe impl Sync for SqliteApi {}

static API: OnceLock<Result<SqliteApi, String>> = OnceLock::new();

pub fn api() -> Result<&'static SqliteApi, String> {
    match API.get_or_init(load_sqlite3mc) {
        Ok(a) => Ok(a),
        Err(e) => Err(e.clone()),
    }
}

/// Locates `sqlite3mc.dll`. Release builds load only from the executable's own folder;
/// debug builds may fall back to `BR_NATIVE_DIR` or the NuGet package cache
/// (`%USERPROFILE%\.nuget\packages\sqlite3mc.pclraw.lib\2.6.1\runtimes\win-x64\native\`).
pub fn find_sqlite3mc_dll() -> Option<PathBuf> {
    const FILE_NAME: &str = "sqlite3mc.dll";
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
                .join("sqlite3mc.pclraw.lib");
            let pinned = pkg_root
                .join("2.6.1")
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

fn load_native_library(path: &Path) -> Result<Library, libloading::Error> {
    let flags = LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR.0 | LOAD_LIBRARY_SEARCH_DEFAULT_DIRS.0;
    // SAFETY: Loading a native DLL with explicit search flags (DLL load dir + default dirs).
    let win_lib = unsafe { WinLibrary::load_with_flags(path, flags) }?;
    Ok(win_lib.into())
}

fn resolve<T: Copy>(lib: &Library, name: &[u8]) -> Result<T, String> {
    // SAFETY: Caller supplies the exact C function pointer type for the SQLite symbol.
    unsafe {
        lib.get::<T>(name)
            .map(|s| *s)
            .map_err(|e| format!("missing symbol {}: {e}", String::from_utf8_lossy(name)))
    }
}

fn load_sqlite3mc() -> Result<SqliteApi, String> {
    let path = find_sqlite3mc_dll()
        .ok_or_else(|| "sqlite3mc.dll not found next to executable".to_string())?;
    let lib = load_native_library(&path)
        .map_err(|e| format!("failed to load {}: {e}", path.display()))?;

    Ok(SqliteApi {
        open_v2: resolve(&lib, b"sqlite3_open_v2\0")?,
        close_v2: resolve(&lib, b"sqlite3_close_v2\0")?,
        prepare_v2: resolve(&lib, b"sqlite3_prepare_v2\0")?,
        step: resolve(&lib, b"sqlite3_step\0")?,
        reset: resolve(&lib, b"sqlite3_reset\0")?,
        clear_bindings: resolve(&lib, b"sqlite3_clear_bindings\0")?,
        finalize: resolve(&lib, b"sqlite3_finalize\0")?,
        bind_null: resolve(&lib, b"sqlite3_bind_null\0")?,
        bind_int: resolve(&lib, b"sqlite3_bind_int\0")?,
        bind_int64: resolve(&lib, b"sqlite3_bind_int64\0")?,
        bind_double: resolve(&lib, b"sqlite3_bind_double\0")?,
        bind_text: resolve(&lib, b"sqlite3_bind_text\0")?,
        bind_blob: resolve(&lib, b"sqlite3_bind_blob\0")?,
        bind_parameter_index: resolve(&lib, b"sqlite3_bind_parameter_index\0")?,
        column_type: resolve(&lib, b"sqlite3_column_type\0")?,
        column_int: resolve(&lib, b"sqlite3_column_int\0")?,
        column_int64: resolve(&lib, b"sqlite3_column_int64\0")?,
        column_double: resolve(&lib, b"sqlite3_column_double\0")?,
        column_text: resolve(&lib, b"sqlite3_column_text\0")?,
        column_blob: resolve(&lib, b"sqlite3_column_blob\0")?,
        column_bytes: resolve(&lib, b"sqlite3_column_bytes\0")?,
        changes: resolve(&lib, b"sqlite3_changes\0")?,
        last_insert_rowid: resolve(&lib, b"sqlite3_last_insert_rowid\0")?,
        errmsg: resolve(&lib, b"sqlite3_errmsg\0")?,
        errcode: resolve(&lib, b"sqlite3_errcode\0")?,
        extended_errcode: resolve(&lib, b"sqlite3_extended_errcode\0")?,
        busy_timeout: resolve(&lib, b"sqlite3_busy_timeout\0")?,
        exec: resolve(&lib, b"sqlite3_exec\0")?,
        free: resolve(&lib, b"sqlite3_free\0")?,
        get_autocommit: resolve(&lib, b"sqlite3_get_autocommit\0")?,
        _lib: lib,
    })
}
