// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use br_contracts::{DataPaths, Settings};
use br_core::crypto::{BlobCipher, CryptoError, HelloInterop, KeyVault, PasswordCheck, UnlockPassword};
use br_core::win32;
use chrono::{Duration as ChronoDuration, Utc};
use windows::core::GUID;
use zeroize::Zeroizing;

fn test_tmp_root() -> PathBuf {
    if let Some(t) = std::env::var_os("BR_TEST_TMP")
        && !t.is_empty()
    {
        return PathBuf::from(t);
    }
    if let Some(t) = std::env::var_os("CARGO_TARGET_DIR")
        && !t.is_empty()
    {
        return PathBuf::from(t).join("test-tmp");
    }
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().and_then(|d| d.parent()).map(|d| d.join("test-tmp")))
        .expect("unable to determine cargo target dir for test temp folder")
}

struct TempHome {
    path: PathBuf,
}

impl TempHome {
    fn new() -> Self {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let id = SEQ.fetch_add(1, Ordering::Relaxed);
        let path = test_tmp_root()
            .join("br-core-crypto")
            .join(format!("h-{}-{id}", std::process::id()));
        fs::create_dir_all(&path).expect("create temp home");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempHome {
    fn drop(&mut self) {
        for _ in 0..5 {
            if !self.path.exists() || fs::remove_dir_all(&self.path).is_ok() {
                return;
            }
            thread::sleep(Duration::from_millis(50));
        }
    }
}

fn rand_bytes(n: usize) -> Vec<u8> {
    let mut v = vec![0u8; n];
    win32::fill_random(&mut v);
    v
}

fn cipher(key: Vec<u8>) -> BlobCipher {
    BlobCipher::new(move || Ok(Zeroizing::new(key.clone())))
}

fn write_blob(c: &BlobCipher, dir: &Path, data: &[u8]) -> PathBuf {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let p = dir.join(format!("blob-{}.enc", SEQ.fetch_add(1, Ordering::Relaxed)));
    let mut w = c.open_write(&p).expect("open_write");
    let mut off = 0;
    let step = 7919;
    while off < data.len() {
        let n = step.min(data.len() - off);
        w.write_all(&data[off..off + n]).expect("write");
        off += n;
    }
    w.finish().expect("finish");
    p
}

fn read_blob(c: &BlobCipher, p: &Path) -> Result<Vec<u8>, String> {
    let mut r = c.open_read(p).map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    r.read_to_end(&mut out).map_err(|e| e.to_string())?;
    Ok(out)
}

fn with_mode(mode: &str) -> Arc<Mutex<Settings>> {
    let mut s = Settings::default();
    s.security.key_protection = mode.to_string();
    Arc::new(Mutex::new(s))
}

fn settings_fn(s: &Arc<Mutex<Settings>>) -> impl Fn() -> Settings + Send + Sync + 'static {
    let c = s.clone();
    move || c.lock().unwrap().clone()
}

// ---- BlobCipherTests (CryptoTests.cs) ----

#[test]
fn round_trip() {
    let h = TempHome::new();
    for size in [0usize, 1, 65_535, 65_536, 65_537, 3 * 65_536, 200_001] {
        let key = rand_bytes(32);
        let data = rand_bytes(size);
        let p = write_blob(&cipher(key.clone()), h.path(), &data);
        assert_eq!(data, read_blob(&cipher(key), &p).unwrap(), "size={size}");
        let chunks = 1.max(size.div_ceil(65_536));
        assert_eq!(
            (20 + size + chunks * 16) as u64,
            fs::metadata(&p).unwrap().len(),
            "size={size}"
        );
    }
}

#[test]
fn ciphertext_does_not_contain_plaintext() {
    let h = TempHome::new();
    let data = "SECRET-PLAINTEXT ".repeat(1000).into_bytes();
    let p = write_blob(&cipher(rand_bytes(32)), h.path(), &data);
    let raw = fs::read(&p).unwrap();
    assert!(!String::from_utf8_lossy(&raw).contains("SECRET"));
}

#[test]
fn tamper_throws() {
    let h = TempHome::new();
    for offset in [5isize, 25, 65_536 + 20 + 3, -1] {
        let key = rand_bytes(32);
        let p = write_blob(&cipher(key.clone()), h.path(), &rand_bytes(150_000));
        let mut bytes = fs::read(&p).unwrap();
        let idx = if offset < 0 {
            bytes.len() - 1
        } else {
            offset as usize
        };
        bytes[idx] ^= 0x01;
        fs::write(&p, &bytes).unwrap();
        assert!(read_blob(&cipher(key), &p).is_err(), "offset={offset}");
    }
}

#[test]
fn truncation_at_chunk_boundary_throws() {
    let h = TempHome::new();
    let key = rand_bytes(32);
    let p = write_blob(&cipher(key.clone()), h.path(), &rand_bytes(3 * 65_536 + 100));
    let bytes = fs::read(&p).unwrap();
    fs::write(&p, &bytes[..(20 + 2 * (65_536 + 16))]).unwrap();
    assert!(read_blob(&cipher(key), &p).is_err());
}

#[test]
fn truncation_mid_chunk_and_wrong_key_throw() {
    let h = TempHome::new();
    let key = rand_bytes(32);
    let p = write_blob(&cipher(key.clone()), h.path(), &rand_bytes(100_000));
    assert!(read_blob(&cipher(rand_bytes(32)), &p).is_err());
    let bytes = fs::read(&p).unwrap();
    fs::write(&p, &bytes[..(bytes.len() - 10)]).unwrap();
    assert!(read_blob(&cipher(key), &p).is_err());
}

#[test]
fn chunk_swap_throws() {
    let h = TempHome::new();
    let key = rand_bytes(32);
    let p = write_blob(&cipher(key.clone()), h.path(), &rand_bytes(3 * 65_536 + 5));
    let mut b = fs::read(&p).unwrap();
    let c = 65_536 + 16;
    let first = b[20..20 + c].to_vec();
    b.copy_within(20 + c..20 + 2 * c, 20);
    b[20 + c..20 + 2 * c].copy_from_slice(&first);
    fs::write(&p, &b).unwrap();
    assert!(read_blob(&cipher(key), &p).is_err());
}

// ---- KeyVaultTests (CryptoTests.cs) ----

#[test]
fn dpapi_creates_and_reloads_same_key() {
    let h = TempHome::new();
    let s = with_mode("dpapi");
    let a = {
        let v = KeyVault::new(h.path(), settings_fn(&s), None, false);
        assert_eq!("dpapi", v.protection());
        assert!(v.try_load_silent().unwrap());
        assert!(v.key_loaded());
        v.derive("x").unwrap().to_vec()
    };
    let b = {
        let v = KeyVault::new(h.path(), settings_fn(&s), None, false);
        assert!(v.try_load_silent().unwrap());
        let x = v.derive("x").unwrap().to_vec();
        assert_ne!(a, v.derive("y").unwrap().to_vec());
        x
    };
    assert_eq!(a, b);
    let file = fs::read(DataPaths::key_file(h.path())).unwrap();
    assert_eq!(b"BRK1", &file[..4]);
    assert_eq!(3, file[4]);
}

#[test]
fn passphrase_mode_waits_then_unlocks_and_rejects_wrong_passphrase() {
    let h = TempHome::new();
    let s = with_mode("passphrase");
    let a = {
        let v = KeyVault::new(h.path(), settings_fn(&s), None, false);
        assert!(!v.try_load_silent().unwrap());
        assert!(v.needs_passphrase());
        assert!(!v.key_loaded());
        assert!(v.unlock_with_passphrase("correct horse"));
        assert!(v.key_loaded());
        let derived = v.derive("x").unwrap().to_vec();
        assert!(!v.unlock_with_passphrase("other"));
        derived
    };
    let v = KeyVault::new(h.path(), settings_fn(&s), None, false);
    assert_eq!("passphrase", v.protection());
    assert!(!v.try_load_silent().unwrap());
    assert!(!v.unlock_with_passphrase("wrong"));
    assert!(!v.key_loaded());
    assert!(v.unlock_with_passphrase("correct horse"));
    assert_eq!(a, v.derive("x").unwrap().to_vec());
}

#[test]
fn switching_dpapi_to_passphrase_keeps_the_key() {
    let h = TempHome::new();
    let s = with_mode("dpapi");
    let v = KeyVault::new(h.path(), settings_fn(&s), None, false);
    assert!(v.try_load_silent().unwrap());
    let a = v.derive("x").unwrap().to_vec();
    s.lock().unwrap().security.key_protection = "passphrase".to_string();
    assert!(v.unlock_with_passphrase("pw-long-enough"));
    assert_eq!("passphrase", v.protection());
    let v2 = KeyVault::new(h.path(), settings_fn(&s), None, false);
    assert!(v2.unlock_with_passphrase("pw-long-enough"));
    assert_eq!(a, v2.derive("x").unwrap().to_vec());
}

#[test]
fn destroy_removes_key_file_and_memory() {
    let h = TempHome::new();
    let s = with_mode("dpapi");
    let v = KeyVault::new(h.path(), settings_fn(&s), None, false);
    assert!(v.try_load_silent().unwrap());
    v.destroy_key().unwrap();
    assert!(!v.key_loaded());
    assert!(!DataPaths::key_file(h.path()).exists());
    assert!(matches!(v.derive("x"), Err(CryptoError::InvalidState(_))));
}

#[test]
fn damaged_key_file_reports_key_lost_and_purge_replaces_it() {
    let h = TempHome::new();
    let s = with_mode("dpapi");
    let kf = DataPaths::key_file(h.path());
    fs::create_dir_all(kf.parent().unwrap()).unwrap();
    fs::write(&kf, [1u8, 2, 3]).unwrap();
    let v = KeyVault::new(h.path(), settings_fn(&s), None, false);
    assert!(matches!(v.try_load_silent(), Err(CryptoError::KeyLost(_))));
    assert!(!v.needs_passphrase());
    v.destroy_key().unwrap();
    assert!(v.try_load_silent().unwrap());
    assert_eq!("dpapi", v.protection());
}

#[test]
fn argon2id_matches_rfc9106_shape_and_is_deterministic() {
    let salt = [0u8; 16];
    let a = KeyVault::argon("pw", &salt, 8192, 1, 1).unwrap();
    let b = KeyVault::argon("pw", &salt, 8192, 1, 1).unwrap();
    assert_eq!(*a, *b);
    assert_ne!(*a, *KeyVault::argon("pw2", &salt, 8192, 1, 1).unwrap());
    assert_eq!(32, a.len());
}

#[test]
fn winrt_parameterized_iid_matches_known_value() {
    assert_eq!(
        GUID::from_u128(0xcdb5efb3_5788_509d_9be1_71ccb8a3362a),
        HelloInterop::parameterized_iid("pinterface({9fc2b0bb-e446-44e2-aa61-9cab8f636af2};b1)")
    );
    assert_eq!(
        GUID::from_u128(0xfd596ffd_2318_558f_9dbe_d21df43764a5),
        HelloInterop::iid_async_op_result()
    );
}

#[test]
fn hello_availability_check_returns_without_prompt() {
    let _available = HelloInterop::check_available();
}

#[test]
fn tpm_key_name_is_per_data_folder_and_never_the_legacy_shared_name() {
    let a = KeyVault::tpm_key_name_for(Path::new(r"C:\Users\x\AppData\Local\BetterRecall"));
    assert!(a.starts_with(&format!("{}.", KeyVault::LEGACY_TPM_KEY_NAME)));
    assert_eq!(KeyVault::LEGACY_TPM_KEY_NAME.len() + 17, a.len());
    assert_eq!(
        a,
        KeyVault::tpm_key_name_for(Path::new(r"c:\users\x\appdata\local\betterrecall\"))
    );
    assert_ne!(
        a,
        KeyVault::tpm_key_name_for(Path::new(r"C:\Users\x\AppData\Local\Temp\h-1"))
    );
}

#[test]
fn destroy_deletes_only_this_folders_own_tpm_key() {
    let own = KeyVault::tpm_key_name_for(Path::new(r"K:\t\h-1"));
    assert_eq!(Some(own.as_str()), KeyVault::owned_tpm_key(&own));
    assert_eq!(None, KeyVault::owned_tpm_key(KeyVault::LEGACY_TPM_KEY_NAME));
}

#[test]
fn tpm_name_is_read_from_a_tpm_key_file_only() {
    let name = b"BetterRecall.DataKeyWrap";
    let mut file = vec![0u8; 7 + name.len() + 4 + 3];
    file[..4].copy_from_slice(b"BRK1");
    file[4] = 1;
    file[5..7].copy_from_slice(&(name.len() as u16).to_le_bytes());
    file[7..7 + name.len()].copy_from_slice(name);
    assert_eq!(
        Some(KeyVault::LEGACY_TPM_KEY_NAME.to_string()),
        KeyVault::tpm_name_in_file(&file)
    );
    file[4] = 3;
    assert_eq!(None, KeyVault::tpm_name_in_file(&file));
    assert_eq!(None, KeyVault::tpm_name_in_file(&[1, 2]));
}

/// Opt in with `BR_TEST_TPM=1`.
#[test]
fn dpapi_destroy_leaves_a_tpm_key_it_does_not_own() {
    if std::env::var("BR_TEST_TPM").ok().as_deref() != Some("1") {
        return;
    }
    let other = format!("BetterRecall.Test.{:08x}", std::process::id());
    win32::tpm_ensure_key(&other).expect("create test tpm key");
    let h = TempHome::new();
    let s = with_mode("dpapi");
    let v = KeyVault::new(h.path(), settings_fn(&s), None, false);
    v.try_load_silent().unwrap();
    v.destroy_key().unwrap();
    let still_exists = win32::tpm_key_exists(&other).unwrap();
    let _ = win32::tpm_delete_key(&other);
    assert!(still_exists);
}

/// Opt in with `BR_TEST_TPM=1`.
#[test]
fn tpm_wrap_round_trip_when_enabled() {
    if std::env::var("BR_TEST_TPM").ok().as_deref() != Some("1") {
        return;
    }
    let h = TempHome::new();
    let s = with_mode("tpm");
    let name = format!("BetterRecall.Test.rt.{:08x}", std::process::id());
    let a = {
        let v = KeyVault::new(h.path(), settings_fn(&s), Some(name.clone()), false);
        assert!(v.try_load_silent().unwrap());
        assert_eq!("tpm", v.protection());
        v.derive("x").unwrap().to_vec()
    };
    {
        let v = KeyVault::new(h.path(), settings_fn(&s), Some(name.clone()), false);
        assert!(v.try_load_silent().unwrap());
        assert_eq!(a, v.derive("x").unwrap().to_vec());
        v.destroy_key().unwrap();
    }
    assert!(!win32::tpm_key_exists(&name).unwrap());
}

// ---- Batch4Tests.cs (UnlockPasswordTests) ----

#[test]
fn set_verify_and_clear() {
    let h = TempHome::new();
    let now = Arc::new(Mutex::new(Utc::now()));
    let now_c = now.clone();
    let pw = UnlockPassword::with_options(
        h.path(),
        Arc::new(move || *now_c.lock().unwrap()),
        1024,
        1,
        1,
    );
    assert!(!pw.is_set());
    assert_eq!(PasswordCheck::NotSet, pw.verify(Some("anything")));
    pw.set("correct horse").unwrap();
    assert!(pw.is_set());
    assert_eq!(PasswordCheck::Ok, pw.verify(Some("correct horse")));
    assert_eq!(PasswordCheck::Wrong, pw.verify(Some("Correct horse")));
    assert_eq!(PasswordCheck::Wrong, pw.verify(Some("")));
    pw.clear().unwrap();
    assert!(!pw.is_set());
    assert!(!DataPaths::unlock_password_file(h.path()).exists());
}

#[test]
fn file_holds_no_password_and_new_salt_each_time() {
    let h = TempHome::new();
    let pw = UnlockPassword::with_options(h.path(), Arc::new(Utc::now), 1024, 1, 1);
    pw.set("correct horse").unwrap();
    let a = fs::read(DataPaths::unlock_password_file(h.path())).unwrap();
    assert!(!String::from_utf8_lossy(&a).contains("correct"));
    let u16_bytes: Vec<u8> = "correct"
        .encode_utf16()
        .flat_map(|c| c.to_le_bytes())
        .collect();
    assert!(!a.windows(u16_bytes.len()).any(|w| w == u16_bytes.as_slice()));
    pw.set("correct horse").unwrap();
    let b = fs::read(DataPaths::unlock_password_file(h.path())).unwrap();
    assert_ne!(a, b);
}

#[test]
fn rate_limit_after_five_wrong_tries_survives_restart_and_doubles() {
    let h = TempHome::new();
    let now = Arc::new(Mutex::new(Utc::now()));
    let mk = || {
        let c = now.clone();
        UnlockPassword::with_options(h.path(), Arc::new(move || *c.lock().unwrap()), 1024, 1, 1)
    };
    let pw = mk();
    pw.set("secret-1").unwrap();
    for _ in 0..4 {
        assert_eq!(PasswordCheck::Wrong, pw.verify(Some("nope")));
    }
    assert_eq!(0, pw.retry_after_seconds());
    assert_eq!(PasswordCheck::Wrong, pw.verify(Some("nope")));
    assert_eq!(30, pw.retry_after_seconds());

    let again = mk();
    assert_eq!(PasswordCheck::Throttled, again.verify(Some("secret-1")));
    *now.lock().unwrap() += ChronoDuration::seconds(31);
    assert_eq!(PasswordCheck::Wrong, again.verify(Some("nope")));
    assert_eq!(60, again.retry_after_seconds());
    *now.lock().unwrap() += ChronoDuration::seconds(61);
    assert_eq!(PasswordCheck::Ok, again.verify(Some("secret-1")));
    assert_eq!(PasswordCheck::Wrong, again.verify(Some("nope")));
    assert_eq!(0, again.retry_after_seconds());
}

#[test]
fn damaged_file_counts_as_not_set() {
    let h = TempHome::new();
    let path = DataPaths::unlock_password_file(h.path());
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, [1u8, 2, 3]).unwrap();
    let pw = UnlockPassword::with_options(h.path(), Arc::new(Utc::now), 1024, 1, 1);
    assert_eq!(PasswordCheck::NotSet, pw.verify(Some("x")));
}

#[test]
fn new_password_rule() {
    assert!(!UnlockPassword::valid_new("12345"));
    assert!(UnlockPassword::valid_new("123456"));
    assert!(!UnlockPassword::valid_new("      "));
}

// ---- Sweep2GTests.cs ----

#[test]
fn passphrase_tries_are_throttled_like_the_unlock_password() {
    let h = TempHome::new();
    let s = with_mode("passphrase");
    {
        let v = KeyVault::new(h.path(), settings_fn(&s), None, false);
        assert!(v.unlock_with_passphrase("correct horse"));
    }
    let vault = KeyVault::new(h.path(), settings_fn(&s), None, false);
    let now = Arc::new(Mutex::new(Utc::now()));
    let now_c = now.clone();
    vault.set_now_for_test(move || *now_c.lock().unwrap());
    for i in 0..UnlockPassword::FREE_TRIES {
        assert!(!vault.unlock_with_passphrase(&format!("wrong {i}")));
    }
    assert_eq!(30, vault.passphrase_retry_after_seconds());
    assert!(!vault.unlock_with_passphrase("correct horse"));
    assert!(!vault.key_loaded());
    *now.lock().unwrap() += ChronoDuration::seconds(31);
    assert_eq!(0, vault.passphrase_retry_after_seconds());
    assert!(vault.unlock_with_passphrase("correct horse"));
    assert_eq!(0, vault.passphrase_retry_after_seconds());
}

#[test]
fn durable_move_replaces_the_target() {
    let h = TempHome::new();
    let a = h.path().join("a.tmp");
    let b = h.path().join("b.bin");
    fs::write(&b, "old").unwrap();
    fs::write(&a, "new").unwrap();
    win32::move_durable(&a, &b).unwrap();
    assert!(!a.exists());
    assert_eq!("new", fs::read_to_string(&b).unwrap());
    assert!(win32::move_durable(&a, &b).is_err());
}

// ---- Sweep2Tests.cs ----

#[test]
fn missing_key_file_next_to_data_is_lost_and_no_new_key_is_written() {
    let h = TempHome::new();
    let s = with_mode("dpapi");
    let v = KeyVault::new(h.path(), settings_fn(&s), None, false);
    fs::write(DataPaths::store_db(h.path()), [1u8, 2, 3]).unwrap();
    assert!(matches!(v.try_load_silent(), Err(CryptoError::KeyLost(_))));
    assert!(!DataPaths::key_file(h.path()).exists());

    s.lock().unwrap().security.key_protection = "passphrase".to_string();
    assert!(!v.needs_passphrase());
    assert!(!v.unlock_with_passphrase("whatever1"));
    assert!(!DataPaths::key_file(h.path()).exists());

    fs::remove_file(DataPaths::store_db(h.path())).unwrap();
    fs::create_dir_all(DataPaths::keyframes_dir(h.path())).unwrap();
    fs::write(DataPaths::keyframes_dir(h.path()).join("2026-11.db"), [1u8]).unwrap();
    s.lock().unwrap().security.key_protection = "dpapi".to_string();
    assert!(matches!(v.try_load_silent(), Err(CryptoError::KeyLost(_))));
}

#[test]
fn truncated_dpapi_key_file_is_lost() {
    for keep in [6usize, 20] {
        let h = TempHome::new();
        let s = with_mode("dpapi");
        {
            let v = KeyVault::new(h.path(), settings_fn(&s), None, false);
            assert!(v.try_load_silent().unwrap());
        }
        let kf = DataPaths::key_file(h.path());
        let bytes = fs::read(&kf).unwrap();
        fs::write(&kf, &bytes[..keep]).unwrap();
        let v = KeyVault::new(h.path(), settings_fn(&s), None, false);
        assert!(matches!(v.try_load_silent(), Err(CryptoError::KeyLost(_))));
        assert!(!v.needs_passphrase());
    }
}

#[test]
fn truncated_passphrase_key_file_is_lost_not_locked() {
    let h = TempHome::new();
    let s = with_mode("passphrase");
    {
        let v = KeyVault::new(h.path(), settings_fn(&s), None, false);
        assert!(v.unlock_with_passphrase("correct horse"));
    }
    let kf = DataPaths::key_file(h.path());
    let bytes = fs::read(&kf).unwrap();
    fs::write(&kf, &bytes[..40]).unwrap();
    let v = KeyVault::new(h.path(), settings_fn(&s), None, false);
    assert!(!v.needs_passphrase());
    assert!(matches!(v.try_load_silent(), Err(CryptoError::KeyLost(_))));
    assert!(!v.unlock_with_passphrase("correct horse"));
}

#[test]
fn payload_checks_per_mode() {
    let f = |mode: u8, payload: &[u8]| -> Vec<u8> {
        let mut v = Vec::with_capacity(5 + payload.len());
        v.extend_from_slice(b"BRK1");
        v.push(mode);
        v.extend_from_slice(payload);
        v
    };
    assert!(!KeyVault::payload_valid(&f(3, &[]), KeyVault::MODE_DPAPI));
    assert!(!KeyVault::payload_valid(
        &f(3, &[10, 0, 0, 0, 1, 2]),
        KeyVault::MODE_DPAPI
    ));
    assert!(KeyVault::payload_valid(
        &f(3, &[2, 0, 0, 0, 1, 2]),
        KeyVault::MODE_DPAPI
    ));
    assert!(!KeyVault::payload_valid(
        &f(1, &[1, 0, b'k', 4, 0, 0, 0, 1]),
        KeyVault::MODE_TPM
    ));
    assert!(KeyVault::payload_valid(
        &f(1, &[1, 0, b'k', 1, 0, 0, 0, 9]),
        KeyVault::MODE_TPM
    ));
    assert!(!KeyVault::payload_valid(
        &f(2, &[0u8; 50]),
        KeyVault::MODE_PASSPHRASE
    ));
    assert!(KeyVault::tpm_transient(0x8028_0400_u32 as i32));
    assert!(!KeyVault::tpm_transient(0x8009_0005_u32 as i32));
}

// ---- Sweep3ICoreTests.cs ----

#[test]
fn b15_lockout_survives_no_clock_step_back() {
    let h = TempHome::new();
    let now = Arc::new(Mutex::new(Utc::now()));
    let now_c = now.clone();
    let pw = UnlockPassword::with_options(
        h.path(),
        Arc::new(move || *now_c.lock().unwrap()),
        1024,
        1,
        1,
    );
    pw.set("open sesame").unwrap();
    for _ in 0..10 {
        let wait = pw.retry_after_seconds() as i64 + 1;
        *now.lock().unwrap() += ChronoDuration::seconds(wait);
        assert_eq!(PasswordCheck::Wrong, pw.verify(Some("nope")));
    }
    assert_eq!(UnlockPassword::MAX_WAIT_SECONDS, pw.retry_after_seconds());
    *now.lock().unwrap() -= ChronoDuration::hours(5);
    assert_eq!(UnlockPassword::MAX_WAIT_SECONDS, pw.retry_after_seconds());
    *now.lock().unwrap() += ChronoDuration::seconds(UnlockPassword::MAX_WAIT_SECONDS as i64 + 1);
    assert_eq!(0, pw.retry_after_seconds());
    assert_eq!(PasswordCheck::Ok, pw.verify(Some("open sesame")));
}

// ---- Sweep3JCoreTests.cs ----

#[test]
fn b14_short_passphrase_neither_creates_nor_rewraps_the_key() {
    let h = TempHome::new();
    let s = with_mode("passphrase");
    {
        let v = KeyVault::new(h.path(), settings_fn(&s), None, false);
        assert!(!v.try_load_silent().unwrap());
        assert!(v.would_set_passphrase());
        assert!(!v.unlock_with_passphrase("x"));
        assert!(!v.key_loaded());
        assert!(v.unlock_with_passphrase("a longer passphrase"));
    }
    let h2 = TempHome::new();
    let s2 = with_mode("dpapi");
    let v2 = KeyVault::new(h2.path(), settings_fn(&s2), None, false);
    assert!(v2.try_load_silent().unwrap());
    s2.lock().unwrap().security.key_protection = "passphrase".to_string();
    assert!(!v2.unlock_with_passphrase("y"));
    assert_eq!(KeyVault::MODE_DPAPI, v2.protection());
    assert!(v2.unlock_with_passphrase("long enough"));
    assert_eq!(KeyVault::MODE_PASSPHRASE, v2.protection());
    assert!(!v2.would_set_passphrase());
    assert!(KeyVault::valid_new_passphrase("12345678"));
    assert!(!KeyVault::valid_new_passphrase("        "));
}

/// RFC 4231 test cases 2 and 6 (short key, key longer than the block).
#[test]
fn hmac_sha256_matches_rfc_4231() {
    let hex = |b: [u8; 32]| b.iter().map(|x| format!("{x:02x}")).collect::<String>();
    assert_eq!(
        "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843",
        hex(br_core::win32::hmac_sha256(b"Jefe", b"what do ya want for nothing?"))
    );
    assert_eq!(
        "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54",
        hex(br_core::win32::hmac_sha256(
            &[0xaa; 131],
            b"Test Using Larger Than Block-Size Key - Hash Key First"
        ))
    );
}
