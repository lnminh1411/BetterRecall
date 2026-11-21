// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Safe wrapper over dynamically loaded `sqlite3mc.dll` (SQLite3MultipleCiphers).

pub mod ffi;

use std::ffi::{CStr, CString, c_int, c_void};
use std::marker::PhantomData;
use std::path::Path;
use std::ptr;

use thiserror::Error;

use ffi::{
    SQLITE_BLOB, SQLITE_DONE, SQLITE_NOTADB, SQLITE_NULL, SQLITE_OK, SQLITE_OPEN_CREATE,
    SQLITE_OPEN_FULLMUTEX, SQLITE_OPEN_READWRITE, SQLITE_ROW, Sqlite3, Sqlite3Stmt, SqliteApi,
    sqlite_transient,
};

#[derive(Debug, Error)]
pub enum SqliteError {
    #[error("SQLite3MC library load failed: {0}")]
    Load(String),
    #[error("SQLite error {code} (extended {extended_code}): {message}")]
    Sqlite {
        code: i32,
        extended_code: i32,
        message: String,
    },
    #[error("SQL or parameter contains an interior NUL byte")]
    NulByte,
    #[error("SQLite column {col} type mismatch: expected {expected}, got type {actual}")]
    TypeMismatch {
        col: i32,
        expected: &'static str,
        actual: i32,
    },
    #[error("SQLite parameter '{0}' not found")]
    MissingParameter(String),
    #[error("SQLite text column is not valid UTF-8")]
    InvalidUtf8,
}

impl SqliteError {
    pub fn code(&self) -> Option<i32> {
        match self {
            Self::Sqlite { code, .. } => Some(*code),
            _ => None,
        }
    }

    pub fn is_not_a_db(&self) -> bool {
        self.code() == Some(SQLITE_NOTADB)
    }
}

/// Parameter value bound to a prepared statement.
#[derive(Debug, Clone, Copy)]
pub enum Value<'a> {
    Null,
    Int(i32),
    Int64(i64),
    Real(f64),
    Text(&'a str),
    Blob(&'a [u8]),
}

impl From<i32> for Value<'_> {
    fn from(v: i32) -> Self {
        Self::Int(v)
    }
}

impl From<i64> for Value<'_> {
    fn from(v: i64) -> Self {
        Self::Int64(v)
    }
}

impl From<f64> for Value<'_> {
    fn from(v: f64) -> Self {
        Self::Real(v)
    }
}

impl<'a> From<&'a str> for Value<'a> {
    fn from(v: &'a str) -> Self {
        Self::Text(v)
    }
}

impl<'a> From<&'a String> for Value<'a> {
    fn from(v: &'a String) -> Self {
        Self::Text(v.as_str())
    }
}

impl<'a> From<&'a [u8]> for Value<'a> {
    fn from(v: &'a [u8]) -> Self {
        Self::Blob(v)
    }
}

impl<'a, T> From<Option<T>> for Value<'a>
where
    T: Into<Value<'a>>,
{
    fn from(v: Option<T>) -> Self {
        match v {
            Some(inner) => inner.into(),
            None => Self::Null,
        }
    }
}

/// Owned SQLite3MC database connection.
pub struct Connection {
    api: &'static SqliteApi,
    db: *mut Sqlite3,
}

// SAFETY: SQLite connections are opened with `SQLITE_OPEN_FULLMUTEX` and externally synchronized by the store.
unsafe impl Send for Connection {}

impl Connection {
    /// Opens or creates a database file at `path` in read-write mode.
    pub fn open(path: &Path) -> Result<Self, SqliteError> {
        let s = path.to_string_lossy();
        Self::open_str(&s, SQLITE_OPEN_READWRITE | SQLITE_OPEN_CREATE | SQLITE_OPEN_FULLMUTEX)
    }

    /// Opens a fresh throwaway in-memory database.
    pub fn open_in_memory() -> Result<Self, SqliteError> {
        Self::open_str(":memory:", SQLITE_OPEN_READWRITE | SQLITE_OPEN_CREATE | SQLITE_OPEN_FULLMUTEX)
    }

    fn open_str(path: &str, flags: c_int) -> Result<Self, SqliteError> {
        let api = ffi::api().map_err(SqliteError::Load)?;
        let c_path = CString::new(path).map_err(|_| SqliteError::NulByte)?;
        let mut db: *mut Sqlite3 = ptr::null_mut();
        let rc = unsafe { (api.open_v2)(c_path.as_ptr(), &mut db, flags, ptr::null()) };
        if rc != SQLITE_OK {
            let err = if !db.is_null() {
                let e = read_error(api, db, rc);
                unsafe {
                    (api.close_v2)(db);
                }
                e
            } else {
                SqliteError::Sqlite {
                    code: rc,
                    extended_code: rc,
                    message: format!("sqlite3_open_v2 failed ({rc})"),
                }
            };
            return Err(err);
        }
        Ok(Self { api, db })
    }

    /// Executes one or more semicolon-separated SQL statements with no bound parameters.
    pub fn exec_batch(&self, sql: &str) -> Result<(), SqliteError> {
        let c_sql = CString::new(sql).map_err(|_| SqliteError::NulByte)?;
        let mut errmsg: *mut std::ffi::c_char = ptr::null_mut();
        let rc = unsafe { (self.api.exec)(self.db, c_sql.as_ptr(), None, ptr::null_mut(), &mut errmsg) };
        if rc != SQLITE_OK {
            let msg = if !errmsg.is_null() {
                let s = unsafe { CStr::from_ptr(errmsg) }.to_string_lossy().into_owned();
                unsafe { (self.api.free)(errmsg.cast::<c_void>()) };
                s
            } else {
                read_errmsg(self.api, self.db)
            };
            let ext = unsafe { (self.api.extended_errcode)(self.db) };
            return Err(SqliteError::Sqlite {
                code: rc,
                extended_code: ext,
                message: msg,
            });
        }
        if !errmsg.is_null() {
            unsafe { (self.api.free)(errmsg.cast::<c_void>()) };
        }
        Ok(())
    }

    /// Prepares a single SQL statement.
    pub fn prepare(&self, sql: &str) -> Result<Statement<'_>, SqliteError> {
        let bytes = sql.as_bytes();
        let mut stmt: *mut Sqlite3Stmt = ptr::null_mut();
        let rc = unsafe {
            (self.api.prepare_v2)(
                self.db,
                bytes.as_ptr().cast::<std::ffi::c_char>(),
                bytes.len() as c_int,
                &mut stmt,
                ptr::null_mut(),
            )
        };
        if rc != SQLITE_OK {
            return Err(read_error(self.api, self.db, rc));
        }
        Ok(Statement {
            api: self.api,
            db: self.db,
            stmt,
            _conn: PhantomData,
        })
    }

    /// Executes a parameterized SQL statement to completion and returns `sqlite3_changes`.
    pub fn execute(&self, sql: &str, params: &[(&str, Value<'_>)]) -> Result<usize, SqliteError> {
        let mut stmt = self.prepare(sql)?;
        stmt.bind_all(params)?;
        stmt.execute()
    }

    /// Runs a query and returns column 0 of the first row as `Option<i64>` (`None` if no row or NULL).
    pub fn query_scalar_i64(
        &self,
        sql: &str,
        params: &[(&str, Value<'_>)],
    ) -> Result<Option<i64>, SqliteError> {
        let mut stmt = self.prepare(sql)?;
        stmt.bind_all(params)?;
        if !stmt.step()? {
            return Ok(None);
        }
        if stmt.is_null(0) {
            return Ok(None);
        }
        Ok(Some(stmt.get_i64(0)?))
    }

    /// Runs a query and returns column 0 of the first row as `Option<String>` (`None` if no row or NULL).
    pub fn query_scalar_string(
        &self,
        sql: &str,
        params: &[(&str, Value<'_>)],
    ) -> Result<Option<String>, SqliteError> {
        let mut stmt = self.prepare(sql)?;
        stmt.bind_all(params)?;
        if !stmt.step()? {
            return Ok(None);
        }
        stmt.get_opt_string(0)
    }

    pub fn changes(&self) -> usize {
        unsafe { (self.api.changes)(self.db).max(0) as usize }
    }

    pub fn last_insert_rowid(&self) -> i64 {
        unsafe { (self.api.last_insert_rowid)(self.db) }
    }

    pub fn busy_timeout(&self, ms: i32) -> Result<(), SqliteError> {
        let rc = unsafe { (self.api.busy_timeout)(self.db, ms as c_int) };
        if rc != SQLITE_OK {
            return Err(read_error(self.api, self.db, rc));
        }
        Ok(())
    }

    /// Returns `true` when the connection is in autocommit mode (i.e. not inside an open transaction).
    pub fn is_autocommit(&self) -> bool {
        unsafe { (self.api.get_autocommit)(self.db) != 0 }
    }

    /// Begins a transaction that automatically rolls back on drop unless `commit` is called.
    pub fn begin_transaction(&self) -> Result<Transaction<'_>, SqliteError> {
        self.exec_batch("BEGIN")?;
        Ok(Transaction {
            conn: self,
            committed: false,
        })
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        if !self.db.is_null() {
            unsafe {
                let _ = (self.api.close_v2)(self.db);
            }
            self.db = ptr::null_mut();
        }
    }
}

/// Scoped transaction on a `Connection`. Rolls back on drop if not committed.
pub struct Transaction<'conn> {
    conn: &'conn Connection,
    committed: bool,
}

impl<'conn> Transaction<'conn> {
    pub fn commit(mut self) -> Result<(), SqliteError> {
        self.conn.exec_batch("COMMIT")?;
        self.committed = true;
        Ok(())
    }

    pub fn rollback(mut self) -> Result<(), SqliteError> {
        self.committed = true;
        self.conn.exec_batch("ROLLBACK")
    }
}

impl Drop for Transaction<'_> {
    fn drop(&mut self) {
        if !self.committed && !self.conn.is_autocommit() {
            let _ = self.conn.exec_batch("ROLLBACK");
        }
    }
}

/// Prepared SQLite statement bound to a `Connection`.
pub struct Statement<'conn> {
    api: &'static SqliteApi,
    db: *mut Sqlite3,
    stmt: *mut Sqlite3Stmt,
    _conn: PhantomData<&'conn Connection>,
}

impl Statement<'_> {
    pub fn bind_named(&mut self, name: &str, val: Value<'_>) -> Result<(), SqliteError> {
        let c_name = CString::new(name).map_err(|_| SqliteError::NulByte)?;
        let idx = unsafe { (self.api.bind_parameter_index)(self.stmt, c_name.as_ptr()) };
        if idx == 0 {
            return Err(SqliteError::MissingParameter(name.to_string()));
        }
        self.bind_index(idx, val)
    }

    pub fn bind_index(&mut self, idx: i32, val: Value<'_>) -> Result<(), SqliteError> {
        let rc = match val {
            Value::Null => unsafe { (self.api.bind_null)(self.stmt, idx as c_int) },
            Value::Int(v) => unsafe { (self.api.bind_int)(self.stmt, idx as c_int, v as c_int) },
            Value::Int64(v) => unsafe { (self.api.bind_int64)(self.stmt, idx as c_int, v) },
            Value::Real(v) => unsafe { (self.api.bind_double)(self.stmt, idx as c_int, v) },
            Value::Text(s) => unsafe {
                (self.api.bind_text)(
                    self.stmt,
                    idx as c_int,
                    s.as_ptr().cast::<std::ffi::c_char>(),
                    s.len() as c_int,
                    sqlite_transient(),
                )
            },
            Value::Blob(b) => unsafe {
                let ptr = if b.is_empty() {
                    ptr::NonNull::<u8>::dangling().as_ptr().cast::<c_void>()
                } else {
                    b.as_ptr().cast::<c_void>()
                };
                (self.api.bind_blob)(
                    self.stmt,
                    idx as c_int,
                    ptr,
                    b.len() as c_int,
                    sqlite_transient(),
                )
            },
        };
        if rc != SQLITE_OK {
            return Err(read_error(self.api, self.db, rc));
        }
        Ok(())
    }

    pub fn bind_all(&mut self, params: &[(&str, Value<'_>)]) -> Result<(), SqliteError> {
        for &(name, val) in params {
            self.bind_named(name, val)?;
        }
        Ok(())
    }

    pub fn reset(&mut self) -> Result<(), SqliteError> {
        let rc = unsafe { (self.api.reset)(self.stmt) };
        if rc != SQLITE_OK {
            return Err(read_error(self.api, self.db, rc));
        }
        Ok(())
    }

    pub fn clear_bindings(&mut self) -> Result<(), SqliteError> {
        let rc = unsafe { (self.api.clear_bindings)(self.stmt) };
        if rc != SQLITE_OK {
            return Err(read_error(self.api, self.db, rc));
        }
        Ok(())
    }

    /// Steps the statement: `Ok(true)` on `SQLITE_ROW`, `Ok(false)` on `SQLITE_DONE`.
    pub fn step(&mut self) -> Result<bool, SqliteError> {
        let rc = unsafe { (self.api.step)(self.stmt) };
        match rc {
            SQLITE_ROW => Ok(true),
            SQLITE_DONE => Ok(false),
            _ => Err(read_error(self.api, self.db, rc)),
        }
    }

    /// Steps to completion (`SQLITE_DONE`) and returns `sqlite3_changes`.
    pub fn execute(&mut self) -> Result<usize, SqliteError> {
        while self.step()? {}
        Ok(unsafe { (self.api.changes)(self.db).max(0) as usize })
    }

    pub fn column_type(&self, col: i32) -> i32 {
        unsafe { (self.api.column_type)(self.stmt, col as c_int) }
    }

    pub fn is_null(&self, col: i32) -> bool {
        self.column_type(col) == SQLITE_NULL
    }

    pub fn get_i32(&self, col: i32) -> Result<i32, SqliteError> {
        Ok(unsafe { (self.api.column_int)(self.stmt, col as c_int) })
    }

    pub fn get_opt_i32(&self, col: i32) -> Result<Option<i32>, SqliteError> {
        if self.is_null(col) {
            Ok(None)
        } else {
            self.get_i32(col).map(Some)
        }
    }

    pub fn get_i64(&self, col: i32) -> Result<i64, SqliteError> {
        Ok(unsafe { (self.api.column_int64)(self.stmt, col as c_int) })
    }

    pub fn get_f64(&self, col: i32) -> Result<f64, SqliteError> {
        Ok(unsafe { (self.api.column_double)(self.stmt, col as c_int) })
    }

    pub fn get_opt_f64(&self, col: i32) -> Result<Option<f64>, SqliteError> {
        if self.is_null(col) {
            Ok(None)
        } else {
            self.get_f64(col).map(Some)
        }
    }

    pub fn get_string(&self, col: i32) -> Result<String, SqliteError> {
        let len = unsafe { (self.api.column_bytes)(self.stmt, col as c_int) };
        if len <= 0 {
            return Ok(String::new());
        }
        let ptr = unsafe { (self.api.column_text)(self.stmt, col as c_int) };
        if ptr.is_null() {
            return Ok(String::new());
        }
        let bytes = unsafe { std::slice::from_raw_parts(ptr, len as usize) };
        std::str::from_utf8(bytes)
            .map(ToOwned::to_owned)
            .map_err(|_| SqliteError::InvalidUtf8)
    }

    pub fn get_opt_string(&self, col: i32) -> Result<Option<String>, SqliteError> {
        if self.is_null(col) {
            Ok(None)
        } else {
            self.get_string(col).map(Some)
        }
    }

    /// Reads a BLOB column. Returns `TypeMismatch` if the stored SQLite value is not `SQLITE_BLOB`
    /// (matches C# `(byte[])rd.GetValue(col)` throwing on non-blob values).
    pub fn get_blob(&self, col: i32) -> Result<Vec<u8>, SqliteError> {
        let ty = self.column_type(col);
        if ty != SQLITE_BLOB {
            return Err(SqliteError::TypeMismatch {
                col,
                expected: "BLOB",
                actual: ty,
            });
        }
        let len = unsafe { (self.api.column_bytes)(self.stmt, col as c_int) };
        if len <= 0 {
            return Ok(Vec::new());
        }
        let ptr = unsafe { (self.api.column_blob)(self.stmt, col as c_int) };
        if ptr.is_null() {
            return Ok(Vec::new());
        }
        let bytes = unsafe { std::slice::from_raw_parts(ptr.cast::<u8>(), len as usize) };
        Ok(bytes.to_vec())
    }
}

impl Drop for Statement<'_> {
    fn drop(&mut self) {
        if !self.stmt.is_null() {
            unsafe {
                let _ = (self.api.finalize)(self.stmt);
            }
            self.stmt = ptr::null_mut();
        }
    }
}

fn read_errmsg(api: &SqliteApi, db: *mut Sqlite3) -> String {
    let ptr = unsafe { (api.errmsg)(db) };
    if ptr.is_null() {
        String::new()
    } else {
        unsafe { CStr::from_ptr(ptr) }.to_string_lossy().into_owned()
    }
}

fn read_error(api: &SqliteApi, db: *mut Sqlite3, rc: c_int) -> SqliteError {
    let code = if rc == SQLITE_OK {
        unsafe { (api.errcode)(db) }
    } else {
        rc
    };
    let extended_code = unsafe { (api.extended_errcode)(db) };
    let message = read_errmsg(api, db);
    SqliteError::Sqlite {
        code,
        extended_code,
        message,
    }
}
