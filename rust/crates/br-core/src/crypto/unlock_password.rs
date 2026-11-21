// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::windows::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use br_contracts::DataPaths;
use chrono::{DateTime, Duration, Utc};
use zeroize::Zeroize;

use crate::crypto::{CryptoError, KeyVault};
use crate::log::Log;
use crate::win32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PasswordCheck {
    Ok,
    Wrong,
    Throttled,
    NotSet,
}

type ClockFn = Arc<dyn Fn() -> DateTime<Utc> + Send + Sync>;

/// Unlock password for `security.unlockMethod = password`: a read gate like Windows Hello, not an encryption key.
///
/// `keys\unlock.bin` holds `"BRU1"`, Argon2id parameters (64 MiB, 3 passes, 4 lanes), a 16-byte salt,
/// the 32-byte hash, the count of wrong tries in a row, and the lockout timestamp (ms since Unix epoch).
pub struct UnlockPassword {
    file: PathBuf,
    now: ClockFn,
    gate: Mutex<()>,
    mem_kib: i32,
    passes: i32,
    lanes: i32,
}

#[derive(Clone)]
struct Record {
    mem_kib: i32,
    passes: i32,
    lanes: i32,
    salt: [u8; 16],
    hash: [u8; 32],
    failures: i32,
    locked_until_ms: i64,
}

impl UnlockPassword {
    pub const MIN_LENGTH: usize = 6;
    pub const MAX_LENGTH: usize = 256;
    pub const FREE_TRIES: i32 = 5;
    /// Longest wait: also the cap on a stored wait, so a clock stepped back cannot extend the lockout (B15).
    pub const MAX_WAIT_SECONDS: i32 = 900;

    const MAGIC: [u8; 4] = *b"BRU1";
    const FILE_LENGTH: usize = 4 + 12 + 16 + 32 + 4 + 8;

    pub fn new(home: &Path) -> Self {
        Self::with_options(home, Arc::new(Utc::now), 65_536, 3, 4)
    }

    pub fn with_options(
        home: &Path,
        now: ClockFn,
        mem_kib: i32,
        passes: i32,
        lanes: i32,
    ) -> Self {
        Self {
            file: DataPaths::unlock_password_file(home),
            now,
            gate: Mutex::new(()),
            mem_kib,
            passes,
            lanes,
        }
    }

    pub fn is_set(&self) -> bool {
        let _g = self.gate.lock().expect("unlock password gate");
        self.read().is_some()
    }

    /// Seconds until the next try is accepted, `0` when tries are open.
    pub fn retry_after_seconds(&self) -> i32 {
        let _g = self.gate.lock().expect("unlock password gate");
        let Some(r) = self.read() else {
            return 0;
        };
        let rec = self.capped(r);
        let wait_ms = rec.locked_until_ms - (self.now)().timestamp_millis();
        if wait_ms > 0 {
            ((wait_ms as f64) / 1000.0).ceil() as i32
        } else {
            0
        }
    }

    pub fn valid_new(password: &str) -> bool {
        let len = password.encode_utf16().count();
        (Self::MIN_LENGTH..=Self::MAX_LENGTH).contains(&len) && !password.trim().is_empty()
    }

    /// Checks a password, counting wrong tries. Throttled tries are refused without running Argon2id.
    pub fn verify(&self, password: Option<&str>) -> PasswordCheck {
        let _g = self.gate.lock().expect("unlock password gate");
        let Some(r) = self.read() else {
            return PasswordCheck::NotSet;
        };
        let rec = self.capped(r);
        if (self.now)().timestamp_millis() < rec.locked_until_ms {
            return PasswordCheck::Throttled;
        }
        let ok = password.is_some_and(|pw| {
            !pw.is_empty() && pw.encode_utf16().count() <= Self::MAX_LENGTH && matches(&rec, pw)
        });
        if ok {
            if rec.failures != 0 || rec.locked_until_ms != 0 {
                let updated = Record {
                    failures: 0,
                    locked_until_ms: 0,
                    ..rec
                };
                let _ = self.write(&updated);
            }
            return PasswordCheck::Ok;
        }
        let failures = rec.failures + 1;
        let mut until = 0i64;
        if failures >= Self::FREE_TRIES {
            let extra = (failures - Self::FREE_TRIES).min(5) as u32;
            let seconds = (30i64 * (1i64 << extra)).min(Self::MAX_WAIT_SECONDS as i64);
            until = ((self.now)() + Duration::seconds(seconds)).timestamp_millis();
        }
        let updated = Record {
            failures,
            locked_until_ms: until,
            ..rec
        };
        let _ = self.write(&updated);
        Log::info(&format!("unlock password wrong ({failures} in a row)"));
        PasswordCheck::Wrong
    }

    /// Sets a new password (fresh salt, counter cleared).
    pub fn set(&self, password: &str) -> Result<(), CryptoError> {
        if !Self::valid_new(password) {
            return Err(CryptoError::InvalidArgument(
                "Password too short or too long.".to_string(),
            ));
        }
        let mut salt = [0u8; 16];
        win32::fill_random(&mut salt);
        let mut hash = KeyVault::argon(password, &salt, self.mem_kib, self.passes, self.lanes)?;
        let mut hash_arr = [0u8; 32];
        hash_arr.copy_from_slice(&hash);
        hash.zeroize();
        {
            let _g = self.gate.lock().expect("unlock password gate");
            self.write(&Record {
                mem_kib: self.mem_kib,
                passes: self.passes,
                lanes: self.lanes,
                salt,
                hash: hash_arr,
                failures: 0,
                locked_until_ms: 0,
            })?;
        }
        hash_arr.zeroize();
        Log::info("unlock password set");
        Ok(())
    }

    pub fn clear(&self) -> io::Result<()> {
        {
            let _g = self.gate.lock().expect("unlock password gate");
            if self.file.exists()
                && let Err(e) = fs::remove_file(&self.file)
            {
                Log::warn_err(
                    "unlock password file delete failed",
                    &format!("{:?}", e.kind()),
                );
                return Err(e);
            }
        }
        Log::info("unlock password removed");
        Ok(())
    }

    fn capped(&self, mut r: Record) -> Record {
        let cap = ((self.now)() + Duration::seconds(Self::MAX_WAIT_SECONDS as i64))
            .timestamp_millis();
        if r.locked_until_ms <= cap {
            return r;
        }
        r.locked_until_ms = cap;
        if let Err(e) = self.write(&r) {
            Log::warn_err(
                "unlock password file write failed",
                &format!("{:?}", e.kind()),
            );
        }
        r
    }

    fn read(&self) -> Option<Record> {
        if !self.file.exists() {
            return None;
        }
        let b = match fs::read(&self.file) {
            Ok(v) => v,
            Err(e) => {
                Log::warn_err("unlock password file unreadable", &format!("{:?}", e.kind()));
                return None;
            }
        };
        if b.len() != Self::FILE_LENGTH || b[..4] != Self::MAGIC {
            Log::warn("unlock password file damaged");
            return None;
        }
        let p = &b[4..];
        let mem = i32::from_le_bytes(p[0..4].try_into().ok()?);
        let passes = i32::from_le_bytes(p[4..8].try_into().ok()?);
        let lanes = i32::from_le_bytes(p[8..12].try_into().ok()?);
        if !(8..=4 * 1024 * 1024).contains(&mem)
            || !(1..=64).contains(&passes)
            || !(1..=64).contains(&lanes)
        {
            Log::warn("unlock password file damaged");
            return None;
        }
        let mut salt = [0u8; 16];
        salt.copy_from_slice(&p[12..28]);
        let mut hash = [0u8; 32];
        hash.copy_from_slice(&p[28..60]);
        let failures = i32::from_le_bytes(p[60..64].try_into().ok()?);
        let locked_until_ms = i64::from_le_bytes(p[64..72].try_into().ok()?);
        Some(Record {
            mem_kib: mem,
            passes,
            lanes,
            salt,
            hash,
            failures,
            locked_until_ms,
        })
    }

    fn write(&self, r: &Record) -> io::Result<()> {
        let mut b = [0u8; Self::FILE_LENGTH];
        b[..4].copy_from_slice(&Self::MAGIC);
        b[4..8].copy_from_slice(&r.mem_kib.to_le_bytes());
        b[8..12].copy_from_slice(&r.passes.to_le_bytes());
        b[12..16].copy_from_slice(&r.lanes.to_le_bytes());
        b[16..32].copy_from_slice(&r.salt);
        b[32..64].copy_from_slice(&r.hash);
        b[64..68].copy_from_slice(&r.failures.to_le_bytes());
        b[68..76].copy_from_slice(&r.locked_until_ms.to_le_bytes());

        if let Some(parent) = self.file.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut tmp_os = self.file.as_os_str().to_os_string();
        tmp_os.push(".tmp");
        let tmp = PathBuf::from(tmp_os);
        {
            let mut f = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .share_mode(0)
                .open(&tmp)?;
            f.write_all(&b)?;
            f.sync_all()?;
        }
        win32::move_replace(&tmp, &self.file)
    }
}

fn matches(r: &Record, password: &str) -> bool {
    let Ok(mut h) = KeyVault::argon(password, &r.salt, r.mem_kib, r.passes, r.lanes) else {
        return false;
    };
    let eq = constant_time_eq(&h, &r.hash);
    h.zeroize();
    eq
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (&x, &y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}
