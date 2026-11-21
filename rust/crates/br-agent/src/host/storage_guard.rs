// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Storage safeguard (`StorageGuard.cs`).

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use br_contracts::{Settings, StorageSettings, StorageUse, full_path};
use br_core::EncryptedStore;
use windows::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
use windows::core::PCWSTR;

use crate::native::to_wide_z;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StorageDecision {
    pub pause: bool,
    pub notify: bool,
    pub deleted: i32,
    pub why: &'static str,
}

/// Minimal store surface for [`StorageGuard`].
pub trait StorageStore: Send + Sync {
    fn get_storage_use(&self) -> StorageUse;
    fn delete_for_space(&self, batch: i32) -> i32;
}

impl StorageStore for EncryptedStore {
    fn get_storage_use(&self) -> StorageUse {
        self.get_storage_use().unwrap_or(StorageUse {
            file_bytes: 0,
            free_bytes: 0,
        })
    }

    fn delete_for_space(&self, batch: i32) -> i32 {
        self.delete_for_space(batch.max(0) as usize).unwrap_or(0) as i32
    }
}

type StorageStoreProvider = Arc<dyn Fn() -> Option<Arc<dyn StorageStore>> + Send + Sync>;

pub struct StorageGuard {
    store: StorageStoreProvider,
    settings: Arc<dyn Fn() -> Settings + Send + Sync>,
    disk_free: Arc<dyn Fn() -> Option<i64> + Send + Sync>,
    paused: AtomicBool,
}

impl StorageGuard {
    pub const GB: i64 = 1024 * 1024 * 1024;
    pub const BATCH: i32 = 500;
    pub const MAX_BATCHES_PER_CHECK: usize = 200;

    pub fn new(
        store: impl Fn() -> Option<Arc<dyn StorageStore>> + Send + Sync + 'static,
        settings: impl Fn() -> Settings + Send + Sync + 'static,
        disk_free: impl Fn() -> Option<i64> + Send + Sync + 'static,
    ) -> Self {
        Self {
            store: Arc::new(store),
            settings: Arc::new(settings),
            disk_free: Arc::new(disk_free),
            paused: AtomicBool::new(false),
        }
    }

    pub fn paused(&self) -> bool {
        self.paused.load(Ordering::Acquire)
    }

    /// Margin above the limits: 1 GB, at most a twentieth of `storage.maxGb`.
    pub fn margin(s: &StorageSettings) -> i64 {
        Self::GB.min((s.max_gb as i64) * Self::GB / 20)
    }

    pub fn low_disk(s: &StorageSettings, disk: Option<i64>, u: StorageUse, margin: i64) -> bool {
        if s.min_free_gb <= 0 {
            return false;
        }
        let Some(d) = disk else { return false };
        d + u.free_bytes < (s.min_free_gb as i64) * Self::GB + margin
    }

    pub fn store_covers_shortfall(
        s: &StorageSettings,
        disk: Option<i64>,
        u: StorageUse,
        margin: i64,
    ) -> bool {
        let Some(d) = disk else { return false };
        u.live_bytes() >= (s.min_free_gb as i64) * Self::GB + margin - (d + u.free_bytes)
    }

    pub fn over_size(s: &StorageSettings, u: StorageUse, margin: i64) -> bool {
        u.live_bytes() > (s.max_gb as i64) * Self::GB - margin
    }

    pub fn check(&self) -> StorageDecision {
        let Some(st) = (self.store)() else {
            return StorageDecision {
                pause: self.paused(),
                notify: false,
                deleted: 0,
                why: "",
            };
        };
        let s = (self.settings)().storage;
        let margin = Self::margin(&s);
        let mut u = st.get_storage_use();
        let mut disk = (self.disk_free)();
        let mut deleted = 0i32;

        if s.overwrite && (Self::low_disk(&s, disk, u, 0) || Self::over_size(&s, u, 0)) {
            let for_disk = Self::store_covers_shortfall(&s, disk, u, margin);
            for _ in 0..Self::MAX_BATCHES_PER_CHECK {
                if !((for_disk && Self::low_disk(&s, disk, u, margin))
                    || Self::over_size(&s, u, margin))
                {
                    break;
                }
                let n = st.delete_for_space(Self::BATCH);
                if n == 0 {
                    break;
                }
                deleted += n;
                u = st.get_storage_use();
                disk = (self.disk_free)();
            }
        }

        let was_paused = self.paused();
        let resume = if was_paused { margin } else { 0 };
        let low = Self::low_disk(&s, disk, u, resume);
        let over = Self::over_size(&s, u, resume);
        let pause = low || over;
        let notify = pause && !was_paused;
        self.paused.store(pause, Ordering::Release);
        StorageDecision {
            pause,
            notify,
            deleted,
            why: if low {
                "disk"
            } else if over {
                "size"
            } else {
                ""
            },
        }
    }

    /// Free bytes of the volume holding `path`, or `None` when unknown (network paths).
    pub fn free_bytes_of(path: &Path) -> Option<i64> {
        let full = full_path(path).ok()?;
        let s = full.to_string_lossy();
        if s.starts_with(r"\\") {
            return None;
        }
        let root = if s.len() >= 3 && s.as_bytes()[1] == b':' {
            format!("{}\\", &s[..2])
        } else {
            return None;
        };
        let w = to_wide_z(&root);
        let mut free_avail = 0u64;
        if unsafe { GetDiskFreeSpaceExW(PCWSTR(w.as_ptr()), Some(&mut free_avail), None, None) }
            .is_ok()
        {
            Some(free_avail.min(i64::MAX as u64) as i64)
        } else {
            None
        }
    }
}
