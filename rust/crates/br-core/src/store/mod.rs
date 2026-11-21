// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Encrypted store, binary vector index, ULID ids, export journal, and zip capture exporter.

pub mod capture_exporter;
pub mod chats;
pub mod encrypted_store;
pub mod export_journal;
pub mod ids;
pub mod storage;
pub mod vector_index;

use std::cell::UnsafeCell;
use std::ops::{Deref, DerefMut};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

use br_contracts::PurgedWindowError;
use thiserror::Error;

use crate::crypto::CryptoError;
use crate::sqlite::SqliteError;

pub use capture_exporter::CaptureExporter;
pub use encrypted_store::EncryptedStore;
pub use export_journal::ExportJournal;
pub use ids::Ids;
pub use vector_index::{BinaryVectorIndex, VectorCandidate, VectorFilter, VectorIndex};

/// Unified error type for store, SQLite3MC, and export operations.
#[derive(Debug, Error)]
pub enum StoreError {
    #[error(transparent)]
    Sqlite(#[from] SqliteError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Crypto(#[from] CryptoError),
    #[error(transparent)]
    PurgedWindow(#[from] PurgedWindowError),
    #[error("Store is disposed.")]
    Disposed,
    #[error("Store is not open.")]
    NotOpen,
    #[error("{0}")]
    KeyLost(String),
    #[error("{0}")]
    InvalidArgument(String),
    #[error("Operation cancelled.")]
    Cancelled,
    #[error("A purge ran during the export; nothing was written.")]
    ExportPurged,
    #[error("Store is locked.")]
    ExportLocked,
    #[error("Export folder not found.")]
    DirectoryNotFound,
    #[error("ZIP error: {0}")]
    Zip(String),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

impl StoreError {
    pub fn is_not_a_db(&self) -> bool {
        matches!(self, Self::Sqlite(e) if e.is_not_a_db())
    }
}

/// A mutual-exclusion lock that also supports bounded `try_lock_for(Duration)`.
pub(crate) struct TimedMutex<T> {
    locked: Mutex<bool>,
    cv: Condvar,
    data: UnsafeCell<T>,
}

// SAFETY: Access to `data` is serialized by `locked`.
unsafe impl<T: Send> Send for TimedMutex<T> {}
unsafe impl<T: Send> Sync for TimedMutex<T> {}

pub(crate) struct TimedMutexGuard<'a, T> {
    mutex: &'a TimedMutex<T>,
}

impl<T> TimedMutex<T> {
    pub fn new(value: T) -> Self {
        Self {
            locked: Mutex::new(false),
            cv: Condvar::new(),
            data: UnsafeCell::new(value),
        }
    }

    pub fn lock(&self) -> TimedMutexGuard<'_, T> {
        let mut guard = self.locked.lock().unwrap_or_else(|e| e.into_inner());
        while *guard {
            guard = self.cv.wait(guard).unwrap_or_else(|e| e.into_inner());
        }
        *guard = true;
        TimedMutexGuard { mutex: self }
    }

    pub fn try_lock_for(&self, timeout: Duration) -> Option<TimedMutexGuard<'_, T>> {
        let deadline = Instant::now() + timeout;
        let mut guard = self.locked.lock().unwrap_or_else(|e| e.into_inner());
        while *guard {
            let now = Instant::now();
            if now >= deadline {
                return None;
            }
            let (g, res) = self
                .cv
                .wait_timeout(guard, deadline - now)
                .unwrap_or_else(|e| e.into_inner());
            guard = g;
            if res.timed_out() && *guard {
                return None;
            }
        }
        *guard = true;
        Some(TimedMutexGuard { mutex: self })
    }
}

impl<T> Deref for TimedMutexGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &Self::Target {
        // SAFETY: Guard holds exclusive ownership of the lock.
        unsafe { &*self.mutex.data.get() }
    }
}

impl<T> DerefMut for TimedMutexGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        // SAFETY: Guard holds exclusive ownership of the lock.
        unsafe { &mut *self.mutex.data.get() }
    }
}

impl<T> Drop for TimedMutexGuard<'_, T> {
    fn drop(&mut self) {
        let mut guard = self.mutex.locked.lock().unwrap_or_else(|e| e.into_inner());
        *guard = false;
        self.mutex.cv.notify_one();
    }
}
