// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Integration tests for `br-installer` (stage I1).
//! Every test runs in a unique subfolder under `BR_TEST_TMP` and a unique
//! `HKCU\Software\BetterRecallTests\<unique>` prefix, cleaned up on drop.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::fmt::Write as _;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering as AtomicOrdering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use br_contracts::{DataPaths, InstallLayout, PipeNames};
use br_fetch::{Catalog, CatalogFile, Component};
use br_installer::{
    APP_SUBKEY, Action, CLEANUP_VALUE_NAME, Ctx, DISPLAY_NAME, InstallError, InstallOpts, LICENSE_ENTRY, ModelMode,
    ModelsResult, ModifyOpts, OLD_PREFIX, Outcome, PUBLISHER, ProcInfo, Progress, RUNONCE_SUBKEY, RUN_SUBKEY,
    SETUP_EXE, SETUP_JSON, STAGING_PREFIX, SetupManifest, Step, TRAILER_LEN, UNINSTALL_SUBKEY,
    URL_INFO_ABOUT, UninstallOpts, append_payload, available_disk_space, close_running,
    compare_versions, create_shortcut, delete_key_tree, exit_code, extract_to_staging,
    extract_to_staging_with_space, install, install_with_progress, install_with_space,
    is_uninstall_temp_file_name, modify_with_progress, parse_args, pick_processes_to_end, read_install_marker,
    read_license_text, read_model_mode, read_reg_dword, read_reg_sz, read_shortcut_target,
    read_trailer, register, remove_shortcut, resolve_models_dir, schedule_old_folder_cleanup, shortcut_path, stub_len,
    swap_into_place, sweep_stale_uninstall_temp, uninstall, uninstall_with_progress, unregister, validate_delete_data_home, verify, write_reg_sz,
};
use sha2::{Digest, Sha256};
use windows::Win32::Foundation::{CloseHandle, ERROR_ALREADY_EXISTS, GetLastError};
use windows::Win32::System::Registry::{HKEY_CURRENT_USER, RegDeleteKeyW};
use windows::Win32::System::Threading::{CREATE_NO_WINDOW, CreateMutexW};
use windows::core::PCWSTR;
use zip::CompressionMethod;
use zip::write::{SimpleFileOptions, ZipWriter};

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

struct TestEnv {
    root: PathBuf,
    reg_prefix: String,
    ctx: Ctx,
}

impl TestEnv {
    fn new(label: &str) -> Self {
        let id = NEXT_ID.fetch_add(1, AtomicOrdering::Relaxed);
        let pid = std::process::id();
        let unique = format!("i1-{label}-{pid}-{id}");
        let base = std::env::var_os("BR_TEST_TMP")
            .filter(|v| !v.to_string_lossy().trim().is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(r"K:\scratch\br-rust\test-tmp"));
        let root = base.join(&unique);
        fs::create_dir_all(&root).expect("create test root");
        let reg_prefix = format!(r"Software\BetterRecallTests\{unique}");
        let ctx = Ctx::for_test(&root, &reg_prefix);
        Self {
            root,
            reg_prefix,
            ctx,
        }
    }

    fn build_installer(
        &self,
        name: &str,
        stub_bytes: &[u8],
        version: &str,
        files: &[(&str, &[u8])],
    ) -> PathBuf {
        let stub_path = self.root.join(format!("{name}.stub.exe"));
        let zip_path = self.root.join(format!("{name}.payload.zip"));
        let out_path = self.root.join(format!("{name}.exe"));

        fs::write(&stub_path, stub_bytes).expect("write stub");
        build_zip(&zip_path, version, files);
        append_payload(&stub_path, &zip_path, &out_path).expect("append_payload");
        out_path
    }
}

impl Drop for TestEnv {
    fn drop(&mut self) {
        let _ = delete_key_tree(&self.reg_prefix);
        // The shared parent goes too once the last test's subtree is gone (RegDeleteKeyW refuses
        // a key that still has subkeys, so a running test keeps it).
        let parent: Vec<u16> = "Software\\BetterRecallTests\0".encode_utf16().collect();
        unsafe {
            let _ = RegDeleteKeyW(HKEY_CURRENT_USER, PCWSTR(parent.as_ptr()));
        }
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn build_zip(path: &Path, version: &str, files: &[(&str, &[u8])]) {
    let file = File::create(path).expect("create zip");
    let mut writer = ZipWriter::new(file);
    let opts = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);

    let total_bytes: u64 = files.iter().map(|(_, data)| data.len() as u64).sum();
    let manifest = SetupManifest {
        version: version.to_string(),
        files: files.len() as u64,
        bytes: total_bytes,
    };
    writer.start_file(SETUP_JSON, opts).expect("start setup.json");
    writer
        .write_all(serde_json::to_string(&manifest).unwrap().as_bytes())
        .expect("write setup.json");

    for (rel, data) in files {
        writer.start_file(*rel, opts).expect("start entry");
        writer.write_all(data).expect("write entry");
    }
    writer.finish().expect("finish zip");
}

fn build_raw_zip(path: &Path, version: &str, raw_entries: &[(&str, &[u8])]) {
    let file = File::create(path).expect("create raw zip");
    let mut writer = ZipWriter::new(file);
    let opts = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);

    let total_bytes: u64 = raw_entries.iter().map(|(_, data)| data.len() as u64).sum();
    let manifest = SetupManifest {
        version: version.to_string(),
        files: raw_entries.len() as u64,
        bytes: total_bytes,
    };
    writer.start_file(SETUP_JSON, opts).expect("start setup.json");
    writer
        .write_all(serde_json::to_string(&manifest).unwrap().as_bytes())
        .expect("write setup.json");

    for (name, data) in raw_entries {
        writer.start_file(*name, opts).expect("start raw entry");
        writer.write_all(data).expect("write raw entry");
    }
    writer.finish().expect("finish raw zip");
}

fn has_prefix_dir(parent: &Path, prefix: &str) -> bool {
    fs::read_dir(parent).is_ok_and(|entries| {
        entries
            .flatten()
            .any(|e| e.file_name().to_string_lossy().starts_with(prefix))
    })
}

// 1. Trailer round trip, flipped payload byte, truncated file, wrong magic.
#[test]
fn trailer_round_trip_and_corruption_checks() {
    let env = TestEnv::new("trailer");
    let stub = b"MZ-FAKE-PE-STUB-BYTES-1234567890";
    let exe = env.build_installer(
        "setup",
        stub,
        "0.3.0-dev",
        &[(InstallLayout::AGENT_EXE, b"agent-binary")],
    );

    let trailer = read_trailer(&exe).expect("read_trailer");
    assert_eq!(stub_len(&trailer), stub.len() as u64);
    verify(&exe, &trailer).expect("verify ok");

    // Flip one payload byte -> damaged error.
    let flipped = env.root.join("flipped.exe");
    let mut bytes = fs::read(&exe).expect("read exe");
    bytes[trailer.offset as usize] ^= 0x01;
    fs::write(&flipped, &bytes).expect("write flipped");
    let err = verify(&flipped, &trailer).expect_err("flipped byte must fail verify");
    assert!(matches!(err, InstallError::Payload(_)));

    // Truncated file (< 64 bytes).
    let truncated = env.root.join("truncated.exe");
    fs::write(&truncated, &bytes[..32]).expect("write truncated");
    let err = read_trailer(&truncated).expect_err("truncated file must fail");
    assert!(matches!(err, InstallError::Payload(_)));

    // Wrong magic.
    let bad_magic = env.root.join("bad-magic.exe");
    let mut magic_bytes = fs::read(&exe).expect("read exe");
    let magic_pos = magic_bytes.len() - TRAILER_LEN as usize;
    magic_bytes[magic_pos] = b'X';
    fs::write(&bad_magic, &magic_bytes).expect("write bad magic");
    let err = read_trailer(&bad_magic).expect_err("wrong magic must fail");
    assert!(matches!(err, InstallError::Payload(_)));

    // Non-zero padding.
    let bad_pad = env.root.join("bad-pad.exe");
    let mut pad_bytes = fs::read(&exe).expect("read exe");
    *pad_bytes.last_mut().unwrap() = 1;
    fs::write(&bad_pad, &pad_bytes).expect("write bad pad");
    let err = read_trailer(&bad_pad).expect_err("non-zero padding must fail");
    assert!(matches!(err, InstallError::Payload(_)));
}

// 2. Zip traversal (`..\evil.txt`) and absolute entry fail extract and leave no staging folder.
#[test]
fn unsafe_zip_entries_fail_and_clean_staging() {
    let env = TestEnv::new("traversal");
    let stub_path = env.root.join("stub.exe");
    fs::write(&stub_path, b"MZ-STUB").expect("write stub");

    for (idx, bad_name) in [r"..\evil.txt", r"C:\evil.txt", "/etc/evil.txt"]
        .iter()
        .enumerate()
    {
        let zip_path = env.root.join(format!("bad-{idx}.zip"));
        let exe_path = env.root.join(format!("bad-{idx}.exe"));
        build_raw_zip(&zip_path, "0.3.0-dev", &[(bad_name, b"nope")]);
        let trailer = append_payload(&stub_path, &zip_path, &exe_path).expect("append");

        let err = extract_to_staging(&env.ctx, &exe_path, &trailer)
            .expect_err("unsafe path must fail extraction");
        assert!(matches!(err, InstallError::Payload(_)));
        assert!(
            !has_prefix_dir(&env.root, STAGING_PREFIX),
            "staging folder leaked after failed extract for {bad_name}"
        );
    }
}

// 3. Fresh install: files in place, `.install` marker correct, `BetterRecall-Setup.exe` == stub bytes.
#[test]
fn fresh_install_places_files_marker_and_stub() {
    let env = TestEnv::new("fresh");
    let stub = b"MZ-STUB-EXACT-BYTES-0123456789ABCDEF";
    let exe = env.build_installer(
        "setup",
        stub,
        "0.3.0-dev",
        &[
            (InstallLayout::AGENT_EXE, b"agent-payload"),
            (InstallLayout::WORKER_EXE, b"worker-payload"),
        ],
    );
    let trailer = read_trailer(&exe).expect("trailer");

    let outcome = install(&env.ctx, &exe, &InstallOpts::default()).expect("install");
    assert_eq!(
        outcome,
        Outcome::Installed {
            version: "0.3.0-dev".to_string(),
            models: ModelsResult::Skipped,
        }
    );

    assert_eq!(
        fs::read(env.ctx.install_dir.join(InstallLayout::AGENT_EXE)).expect("agent"),
        b"agent-payload"
    );
    assert_eq!(
        fs::read(env.ctx.install_dir.join(InstallLayout::WORKER_EXE)).expect("worker"),
        b"worker-payload"
    );

    let marker = read_install_marker(&env.ctx.install_dir).expect(".install marker");
    assert_eq!(marker.version, "0.3.0-dev");
    assert_eq!(marker.sha256_hex, trailer.sha256_hex());
    assert_eq!(marker.file_count, 2);

    let installed_setup = fs::read(env.ctx.install_dir.join(SETUP_EXE)).expect("setup exe");
    assert_eq!(installed_setup, stub);
}

// 4. Upgrade replaces old install completely; no `.BetterRecall.old-*` or staging folder remains.
#[test]
fn upgrade_replaces_old_install_and_cleans_up() {
    let env = TestEnv::new("upgrade");
    let v1 = env.build_installer(
        "v1",
        b"MZ-STUB-V1",
        "0.3.0-dev",
        &[
            (InstallLayout::AGENT_EXE, b"agent-v1"),
            ("obsolete.dll", b"old-extra-file"),
        ],
    );
    install(&env.ctx, &v1, &InstallOpts::default()).expect("install v1");
    assert!(env.ctx.install_dir.join("obsolete.dll").exists());

    let v2 = env.build_installer(
        "v2",
        b"MZ-STUB-V2",
        "0.3.0",
        &[(InstallLayout::AGENT_EXE, b"agent-v2")],
    );
    let outcome = install(&env.ctx, &v2, &InstallOpts::default()).expect("upgrade v2");
    assert_eq!(
        outcome,
        Outcome::Installed {
            version: "0.3.0".to_string(),
            models: ModelsResult::Skipped,
        }
    );

    assert!(!env.ctx.install_dir.join("obsolete.dll").exists());
    assert_eq!(
        fs::read(env.ctx.install_dir.join(InstallLayout::AGENT_EXE)).expect("agent v2"),
        b"agent-v2"
    );
    assert!(!has_prefix_dir(&env.root, OLD_PREFIX));
    assert!(!has_prefix_dir(&env.root, STAGING_PREFIX));
}

// 5. Rollback: open file without FILE_SHARE_DELETE fails swap with InUse; old untouched, staging gone.
#[test]
fn locked_file_in_old_install_fails_swap_with_in_use() {
    let env = TestEnv::new("rollback");
    let v1 = env.build_installer(
        "v1",
        b"MZ-STUB-V1",
        "0.3.0-dev",
        &[(InstallLayout::AGENT_EXE, b"agent-v1-original")],
    );
    install(&env.ctx, &v1, &InstallOpts::default()).expect("install v1");

    // Hold a file in the old install open without FILE_SHARE_DELETE (share_mode = 0).
    let locked_path = env.ctx.install_dir.join(InstallLayout::AGENT_EXE);
    let _lock = OpenOptions::new()
        .read(true)
        .share_mode(0)
        .open(&locked_path)
        .expect("lock agent exe");

    let v2 = env.build_installer(
        "v2",
        b"MZ-STUB-V2",
        "0.3.0",
        &[(InstallLayout::AGENT_EXE, b"agent-v2-new")],
    );
    let trailer = read_trailer(&v2).expect("trailer");
    let staged = extract_to_staging(&env.ctx, &v2, &trailer).expect("extract");
    let err = swap_into_place(&env.ctx, staged).expect_err("swap must fail while file locked");
    assert!(matches!(err, InstallError::InUse { .. }));

    drop(_lock);
    assert_eq!(
        fs::read(&locked_path).expect("old file untouched"),
        b"agent-v1-original"
    );
    assert!(!has_prefix_dir(&env.root, STAGING_PREFIX));
    assert!(!has_prefix_dir(&env.root, OLD_PREFIX));
}

// N2 (sweep 6): an upgrade refused because the old install is in use restarts the agent that
// was running instead of leaving BetterRecall stopped.
#[test]
fn n2_refused_upgrade_restarts_the_running_agent() {
    use windows::Win32::System::Threading::CreateEventW;
    let env = TestEnv::new("n2-restart");
    // Stage a real small Windows executable as BetterRecall.Agent.exe so RestartAgentGuard's
    // CreateProcessW actually launches it (fake bytes would fail with ERROR_BAD_EXE_FORMAT).
    // Lock a sibling file instead of the agent exe so swap_into_place fails with InUse while
    // BetterRecall.Agent.exe remains executable.
    let ping_bytes = fs::read(r"C:\Windows\System32\PING.EXE").expect("read PING.EXE");
    let v1 = env.build_installer(
        "v1",
        b"MZ-STUB-V1",
        "0.3.0-dev",
        &[
            (InstallLayout::AGENT_EXE, ping_bytes.as_slice()),
            ("lock.bin", b"hold"),
        ],
    );
    install(&env.ctx, &v1, &InstallOpts::default()).expect("install v1");

    // An agent "runs" for this data folder: its scoped quit event exists.
    let name: Vec<u16> = br_contracts::PipeNames::agent_quit_event_for(&env.ctx.home)
        .encode_utf16()
        .chain(Some(0))
        .collect();
    let ev = unsafe { CreateEventW(None, true, false, PCWSTR(name.as_ptr())) }.expect("quit event");

    let agent_path = env.ctx.install_dir.join(InstallLayout::AGENT_EXE);
    let lock_path = env.ctx.install_dir.join("lock.bin");
    let lock = OpenOptions::new().read(true).share_mode(0).open(&lock_path).expect("lock");
    let v2 = env.build_installer(
        "v2",
        b"MZ-STUB-V2",
        "0.3.0",
        &[(InstallLayout::AGENT_EXE, b"agent-v2-new")],
    );
    let err = install(&env.ctx, &v2, &InstallOpts::default()).expect_err("in use");
    drop(lock);
    unsafe {
        let _ = windows::Win32::Foundation::CloseHandle(ev);
    }
    assert!(matches!(err, InstallError::InUse { .. }), "{err:?}");
    let log = fs::read_to_string(env.ctx.home.join("logs").join("setup.log")).expect("setup.log");
    assert!(
        log.contains("restarting the agent that was running"),
        "setup.log must show the restart attempt:\n{log}"
    );
    assert!(
        log.contains(&format!("starting {}", agent_path.display())),
        "setup.log must show start_agent spawning the staged agent:\n{log}"
    );
    assert!(
        !log.contains("agent restart failed"),
        "start_agent must succeed on the staged executable:\n{log}"
    );
    assert_eq!(fs::read(&agent_path).expect("old agent"), ping_bytes);
}

// 6. Registry values written exactly as listed; unregister removes them; unrelated Run value survives.
#[test]
fn registry_register_and_unregister_preserves_other_run_value() {
    let env = TestEnv::new("registry");
    fs::create_dir_all(&env.ctx.install_dir).expect("create install_dir");
    fs::write(env.ctx.install_dir.join("data.bin"), vec![0u8; 2048]).expect("write file");

    register(&env.ctx, "0.3.0-dev").expect("register");

    let uninstall_key = env.ctx.reg_key(UNINSTALL_SUBKEY);
    let run_key = env.ctx.reg_key(RUN_SUBKEY);
    let app_key = env.ctx.reg_key(APP_SUBKEY);

    let install_str = env.ctx.install_dir.to_string_lossy().into_owned();
    let agent_exe = env.ctx.install_dir.join(InstallLayout::AGENT_EXE);
    let setup_exe = env.ctx.install_dir.join(SETUP_EXE);

    assert_eq!(
        read_reg_sz(&uninstall_key, "DisplayName").unwrap().as_deref(),
        Some(DISPLAY_NAME)
    );
    assert_eq!(
        read_reg_sz(&uninstall_key, "DisplayVersion")
            .unwrap()
            .as_deref(),
        Some("0.3.0-dev")
    );
    assert_eq!(
        read_reg_sz(&uninstall_key, "Publisher").unwrap().as_deref(),
        Some(PUBLISHER)
    );
    assert_eq!(
        read_reg_sz(&uninstall_key, "InstallLocation")
            .unwrap()
            .as_deref(),
        Some(install_str.as_str())
    );
    assert_eq!(
        read_reg_sz(&uninstall_key, "DisplayIcon").unwrap().as_deref(),
        Some(format!("\"{}\",0", agent_exe.display()).as_str())
    );
    assert_eq!(
        read_reg_sz(&uninstall_key, "UninstallString")
            .unwrap()
            .as_deref(),
        Some(format!("\"{}\" /uninstall", setup_exe.display()).as_str())
    );
    assert_eq!(
        read_reg_sz(&uninstall_key, "QuietUninstallString")
            .unwrap()
            .as_deref(),
        Some(format!("\"{}\" /uninstall /S", setup_exe.display()).as_str())
    );
    assert_eq!(
        read_reg_sz(&uninstall_key, "ModifyPath").unwrap().as_deref(),
        Some(format!("\"{}\" /modify", setup_exe.display()).as_str())
    );
    assert_eq!(read_reg_dword(&uninstall_key, "NoRepair").unwrap(), Some(1));
    assert_eq!(
        read_reg_dword(&uninstall_key, "EstimatedSize").unwrap(),
        Some(2)
    );
    assert_eq!(
        read_reg_sz(&uninstall_key, "URLInfoAbout")
            .unwrap()
            .as_deref(),
        Some(URL_INFO_ABOUT)
    );
    assert_eq!(
        read_reg_sz(&run_key, "BetterRecall").unwrap().as_deref(),
        Some(format!("\"{}\"", agent_exe.display()).as_str())
    );
    assert_eq!(
        read_reg_sz(&app_key, "InstallPath").unwrap().as_deref(),
        Some(install_str.as_str())
    );

    unregister(&env.ctx).expect("unregister");
    assert_eq!(read_reg_sz(&uninstall_key, "DisplayName").unwrap(), None);
    assert_eq!(read_reg_sz(&app_key, "InstallPath").unwrap(), None);
    assert_eq!(read_reg_sz(&run_key, "BetterRecall").unwrap(), None);

    // A Run value pointing elsewhere must survive unregister.
    let other_run = r#""C:\OtherFolder\BetterRecall.exe""#;
    write_reg_sz(&run_key, "BetterRecall", other_run).expect("write other run");
    unregister(&env.ctx).expect("unregister with foreign Run");
    assert_eq!(
        read_reg_sz(&run_key, "BetterRecall").unwrap().as_deref(),
        Some(other_run)
    );
}

// 7. Downgrade refused by default, allowed with allow_downgrade; 0.3.0-dev < 0.3.0 < 0.3.1.
#[test]
fn downgrade_refused_unless_allowed_and_version_ordering() {
    assert_eq!(compare_versions("0.3.0-dev", "0.3.0"), Ordering::Less);
    assert_eq!(compare_versions("0.3.0", "0.3.1"), Ordering::Less);
    assert_eq!(compare_versions("0.3.0-dev", "0.3.1"), Ordering::Less);
    assert_eq!(compare_versions("0.3.1", "0.3.0"), Ordering::Greater);
    assert_eq!(compare_versions("0.3.0", "0.3.0"), Ordering::Equal);

    let env = TestEnv::new("downgrade");
    let newer = env.build_installer(
        "v031",
        b"MZ-STUB",
        "0.3.1",
        &[(InstallLayout::AGENT_EXE, b"v031")],
    );
    install(&env.ctx, &newer, &InstallOpts::default()).expect("install 0.3.1");

    let older = env.build_installer(
        "v030",
        b"MZ-STUB",
        "0.3.0",
        &[(InstallLayout::AGENT_EXE, b"v030")],
    );
    let err = install(&env.ctx, &older, &InstallOpts::default())
        .expect_err("downgrade must be refused");
    assert!(matches!(err, InstallError::Downgrade { .. }));

    let outcome = install(
        &env.ctx,
        &older,
        &InstallOpts {
            allow_downgrade: true,
            ..Default::default()
        },
    )
    .expect("allow_downgrade permits downgrade");
    assert_eq!(
        outcome,
        Outcome::Installed {
            version: "0.3.0".to_string(),
            models: ModelsResult::Skipped,
        }
    );
    assert_eq!(
        fs::read(env.ctx.install_dir.join(InstallLayout::AGENT_EXE)).expect("agent"),
        b"v030"
    );
}

// 8. Not enough space with an injected free-space value.
#[test]
fn not_enough_space_rejected_before_extract() {
    let env = TestEnv::new("space");
    let payload_data = vec![b'A'; 4096];
    let exe = env.build_installer(
        "setup",
        b"MZ-STUB",
        "0.3.0-dev",
        &[(InstallLayout::AGENT_EXE, &payload_data)],
    );
    let trailer = read_trailer(&exe).expect("trailer");

    let err = extract_to_staging_with_space(&env.ctx, &exe, &trailer, |_| Ok(100))
        .expect_err("low space must fail");
    assert!(matches!(err, InstallError::NotEnoughSpace { .. }));
    assert!(!has_prefix_dir(&env.root, STAGING_PREFIX));

    let err = install_with_space(&env.ctx, &exe, &InstallOpts::default(), |_| Ok(4096))
        .expect_err("exact byte count without 10% margin must fail");
    assert!(matches!(err, InstallError::NotEnoughSpace { .. }));
}

// 9. Shortcut created in test start-menu folder pointing at agent exe; removed by remove_shortcut.
#[test]
fn shortcut_create_read_and_remove() {
    let env = TestEnv::new("shortcut");
    fs::create_dir_all(&env.ctx.install_dir).expect("create install_dir");
    let agent = env.ctx.install_dir.join(InstallLayout::AGENT_EXE);
    fs::write(&agent, b"dummy").expect("write dummy agent");

    let lnk = create_shortcut(&env.ctx).expect("create_shortcut");
    assert_eq!(lnk, shortcut_path(&env.ctx));
    assert!(lnk.exists());

    let target = read_shortcut_target(&lnk).expect("read_shortcut_target");
    assert!(
        target
            .to_string_lossy()
            .eq_ignore_ascii_case(&agent.to_string_lossy()),
        "expected shortcut target {}, got {}",
        agent.display(),
        target.display()
    );

    remove_shortcut(&env.ctx).expect("remove_shortcut");
    assert!(!lnk.exists());
    // Idempotent when already removed.
    remove_shortcut(&env.ctx).expect("remove_shortcut idempotent");
}

// 10. pick_processes_to_end: tree membership, other session, sibling prefix, outside image, no agent pid.
#[test]
fn pick_processes_to_end_filters_correctly() {
    let install_dir = Path::new(r"C:\Users\alice\AppData\Local\Programs\BetterRecall");
    let procs = vec![
        // Agent itself (root of tree).
        ProcInfo {
            pid: 100,
            parent_pid: 1,
            session: 1,
            image: Some(install_dir.join("BetterRecall.Agent.exe")),
        },
        // Worker child in install_dir.
        ProcInfo {
            pid: 101,
            parent_pid: 100,
            session: 1,
            image: Some(install_dir.join(r"worker\BetterRecall.Worker.exe")),
        },
        // Grandchild in install_dir (different case).
        ProcInfo {
            pid: 102,
            parent_pid: 101,
            session: 1,
            image: Some(PathBuf::from(
                r"c:\users\alice\appdata\local\programs\betterrecall\ui\BetterRecall.UI.exe",
            )),
        },
        // Child whose image is outside install_dir -> must be excluded.
        ProcInfo {
            pid: 103,
            parent_pid: 100,
            session: 1,
            image: Some(PathBuf::from(r"C:\Windows\System32\cmd.exe")),
        },
        // Child with no readable image -> must be excluded.
        ProcInfo {
            pid: 104,
            parent_pid: 100,
            session: 1,
            image: None,
        },
        // Child in another session -> must be excluded.
        ProcInfo {
            pid: 105,
            parent_pid: 100,
            session: 2,
            image: Some(install_dir.join("BetterRecall.Agent.exe")),
        },
        // Sibling folder with shared prefix (`BetterRecall2`) -> must be excluded.
        ProcInfo {
            pid: 106,
            parent_pid: 100,
            session: 1,
            image: Some(PathBuf::from(
                r"C:\Users\alice\AppData\Local\Programs\BetterRecall2\BetterRecall.Agent.exe",
            )),
        },
        // Unrelated process running from install_dir (not in agent's tree).
        ProcInfo {
            pid: 200,
            parent_pid: 1,
            session: 1,
            image: Some(install_dir.join("betterrecall.exe")),
        },
    ];

    let with_agent = pick_processes_to_end(&procs, Some(100), install_dir, 1);
    assert_eq!(with_agent, vec![100, 101, 102]);

    // Without an agent pid, select only processes in this session whose image is under install_dir.
    let without_agent = pick_processes_to_end(&procs, None, install_dir, 1);
    assert_eq!(without_agent, vec![100, 101, 102, 200]);
}

// 11. Ctx::for_test panics for a prefix outside Software\BetterRecallTests\ and for the default home.
#[test]
#[should_panic(expected = "invalid Ctx::for_test parameters")]
fn ctx_for_test_panics_on_bad_reg_prefix() {
    let env = TestEnv::new("bad-prefix");
    let _ = Ctx::for_test(&env.root, r"Software\BetterRecall");
}

#[test]
#[should_panic(expected = "default BetterRecall data folder")]
fn ctx_for_test_panics_on_default_home() {
    let default_home = Ctx::production().home;
    // Only if BETTERRECALL_HOME is not overriding default_home; compute %LOCALAPPDATA%\BetterRecall directly.
    let local_app = std::env::var_os("LOCALAPPDATA")
        .map(|v| PathBuf::from(v).join("BetterRecall"))
        .unwrap_or(default_home);
    let _ = Ctx::for_test(&local_app, r"Software\BetterRecallTests\should-panic");
}

// 12. Uninstall removes folder, registry entries, and shortcut, and leaves test home in place.
#[test]
fn uninstall_cleans_install_reg_and_shortcut_but_keeps_home() {
    let env = TestEnv::new("uninstall");
    fs::create_dir_all(&env.ctx.home).expect("create test home");
    let user_note = env.ctx.home.join("keep-me.txt");
    fs::write(&user_note, b"user data").expect("write home file");

    let exe = env.build_installer(
        "setup",
        b"MZ-STUB",
        "0.3.0-dev",
        &[(InstallLayout::AGENT_EXE, b"agent")],
    );
    install(&env.ctx, &exe, &InstallOpts::default()).expect("install");
    assert!(env.ctx.install_dir.exists());
    assert!(shortcut_path(&env.ctx).exists());

    let outcome = uninstall(&env.ctx, &UninstallOpts::default()).expect("uninstall");
    assert_eq!(
        outcome,
        Outcome::Uninstalled {
            reboot_required: false
        }
    );

    assert!(!env.ctx.install_dir.exists());
    assert!(!shortcut_path(&env.ctx).exists());
    assert_eq!(
        read_reg_sz(&env.ctx.reg_key(UNINSTALL_SUBKEY), "DisplayName").unwrap(),
        None
    );
    assert_eq!(
        read_reg_sz(&env.ctx.reg_key(RUN_SUBKEY), "BetterRecall").unwrap(),
        None
    );
    assert_eq!(
        read_reg_sz(&env.ctx.reg_key(APP_SUBKEY), "InstallPath").unwrap(),
        None
    );

    // Test home folder and its data remain intact.
    assert!(env.ctx.home.exists());
    assert_eq!(fs::read(&user_note).expect("home file preserved"), b"user data");
    assert!(env.ctx.home.join(r"logs\setup.log").exists());
}

#[test]
fn close_running_on_idle_test_home_returns_quickly() {
    let env = TestEnv::new("idle-close");
    let start = Instant::now();
    close_running(&env.ctx).expect("close_running");
    assert!(
        start.elapsed().as_secs() < 2,
        "close_running took too long on idle home: {:?}",
        start.elapsed()
    );
}

#[test]
fn cli_parse_args_handles_flags_and_errors() {
    let a = parse_args(["/S", "/norun", "/AllowDowngrade"]).expect("parse slash");
    assert!(a.silent);
    assert_eq!(a.action, Action::Install);
    assert!(a.no_run);
    assert!(a.allow_downgrade);
    assert!(!a.relaunched);

    let b = parse_args(["--uninstall", "-s", "--relaunched"]).expect("parse dash");
    assert!(b.silent);
    assert_eq!(b.action, Action::Uninstall);
    assert!(b.relaunched);

    let c = parse_args([
        "/modify",
        "--dev-root",
        r"K:\scratch\test",
        "--dev-reg-prefix",
        r"Software\BetterRecallTests\cli",
    ])
    .expect("parse dev");
    assert_eq!(c.action, Action::Modify);
    assert_eq!(c.dev_root, Some(PathBuf::from(r"K:\scratch\test")));
    assert_eq!(
        c.dev_reg_prefix.as_deref(),
        Some(r"Software\BetterRecallTests\cli")
    );

    let err = parse_args(["/unknown"]).expect_err("unknown flag");
    assert_eq!(err.exit_code(), 2);

    let err = parse_args(["/uninstall", "/modify"]).expect_err("conflicting actions");
    assert_eq!(err.exit_code(), 2);

    let err = parse_args(["--dev-root", r"K:\scratch\test"]).expect_err("missing dev-reg-prefix");
    assert_eq!(err.exit_code(), 2);

    // I2 flags: /models=full|minimal|later and /deletedata.
    let m = parse_args(["/S", "/models=Full"]).expect("parse /models=Full");
    assert_eq!(m.model_mode, Some(ModelMode::Full));
    assert!(!m.delete_data);
    let l = parse_args(["/S", "/models=later"]).expect("parse /models=later");
    assert_eq!(l.model_mode, Some(ModelMode::Later));

    let d = parse_args(["/uninstall", "/S", "/deletedata"]).expect("parse /deletedata");
    assert_eq!(d.action, Action::Uninstall);
    assert!(d.delete_data);

    let err = parse_args(["/deletedata"]).expect_err("/deletedata without /uninstall");
    assert_eq!(err.exit_code(), exit_code::USAGE);

    let err = parse_args(["/models=bogus"]).expect_err("invalid /models value");
    assert_eq!(err.exit_code(), exit_code::USAGE);
}

// --- I2 integration tests ---

// 14. Cancel during extraction: Cancelled, no staging folder, existing install untouched.
#[test]
fn cancel_during_extraction_cleans_staging_and_leaves_existing_install_untouched() {
    struct CancelOnBytes {
        cancelled: AtomicBool,
    }
    impl Progress for CancelOnBytes {
        fn step(&self, _step: Step) {}
        fn bytes(&self, done: u64, _total: u64) {
            if done > 0 {
                self.cancelled.store(true, AtomicOrdering::SeqCst);
            }
        }
        fn cancelled(&self) -> bool {
            self.cancelled.load(AtomicOrdering::SeqCst)
        }
    }

    let env = TestEnv::new("cancel-extract");
    let v1 = env.build_installer(
        "v1",
        b"MZ-STUB-V1",
        "0.3.0-dev",
        &[(InstallLayout::AGENT_EXE, b"agent-v1-keep")],
    );
    install(&env.ctx, &v1, &InstallOpts::default()).expect("install v1");

    let v2 = env.build_installer(
        "v2",
        b"MZ-STUB-V2",
        "0.3.0",
        &[
            ("a.bin", b"first-entry-bytes"),
            ("b.bin", b"second-entry-bytes"),
            (InstallLayout::AGENT_EXE, b"agent-v2-new"),
        ],
    );
    let progress = CancelOnBytes {
        cancelled: AtomicBool::new(false),
    };
    let err = install_with_progress(
        &env.ctx,
        &v2,
        &InstallOpts::default(),
        available_disk_space,
        &progress,
    )
    .expect_err("install must cancel during extraction");
    assert!(matches!(err, InstallError::Cancelled));
    assert_eq!(err.exit_code(), exit_code::CANCELLED);
    assert!(!has_prefix_dir(&env.root, STAGING_PREFIX));
    assert_eq!(
        fs::read(env.ctx.install_dir.join(InstallLayout::AGENT_EXE)).expect("v1 intact"),
        b"agent-v1-keep"
    );
}

// 15. Delete-data uninstall removes only allowlisted entries; preserves app\ and unknown files;
// removes home itself when only allowlisted entries were present.
#[test]
fn uninstall_delete_data_removes_only_allowlisted_entries() {
    let populate_listed = |home: &Path| {
        for dir in ["keys", "keyframes", "replays", "models", "cache", "logs"] {
            fs::create_dir_all(home.join(dir)).expect("create listed dir");
            fs::write(home.join(dir).join("placeholder.txt"), b"x").expect("write placeholder");
        }
        fs::write(home.join("store.db"), b"fake").expect("write fake store.db");
        fs::write(home.join("store.db-wal"), b"fake").expect("write fake wal");
        fs::write(home.join("store.db-shm"), b"fake").expect("write fake shm");
        fs::write(home.join("settings.json"), b"{}").expect("write settings.json");
        fs::write(home.join("agent.pid"), b"12345").expect("write agent.pid");
    };

    // Case A: listed entries + app\x.exe + notes.txt -> listed gone, app\ and notes.txt kept, home kept.
    let env_keep = TestEnv::new("deldata-keep");
    fs::create_dir_all(&env_keep.ctx.home).expect("create home");
    populate_listed(&env_keep.ctx.home);
    fs::create_dir_all(env_keep.ctx.home.join("app")).expect("create app");
    fs::write(env_keep.ctx.home.join(r"app\x.exe"), b"old-launcher").expect("write app\\x.exe");
    fs::write(env_keep.ctx.home.join("notes.txt"), b"user-notes").expect("write notes.txt");

    let exe_a = env_keep.build_installer(
        "setup",
        b"MZ-STUB",
        "0.3.0-dev",
        &[(InstallLayout::AGENT_EXE, b"agent")],
    );
    install(&env_keep.ctx, &exe_a, &InstallOpts::default()).expect("install");
    uninstall(
        &env_keep.ctx,
        &UninstallOpts {
            delete_data: true,
            ..Default::default()
        },
    )
    .expect("uninstall delete_data");

    for gone in [
        "keys",
        "store.db",
        "store.db-wal",
        "store.db-shm",
        "keyframes",
        "replays",
        "models",
        "cache",
        "logs",
        "settings.json",
        "agent.pid",
    ] {
        assert!(
            !env_keep.ctx.home.join(gone).exists(),
            "expected {gone} to be deleted"
        );
    }
    assert!(env_keep.ctx.home.exists(), "home must remain when unknown files exist");
    assert_eq!(
        fs::read(env_keep.ctx.home.join(r"app\x.exe")).expect("app\\x.exe preserved"),
        b"old-launcher"
    );
    assert_eq!(
        fs::read(env_keep.ctx.home.join("notes.txt")).expect("notes.txt preserved"),
        b"user-notes"
    );

    // Case B: only listed entries -> home folder itself is removed.
    let env_empty = TestEnv::new("deldata-empty");
    fs::create_dir_all(&env_empty.ctx.home).expect("create home");
    populate_listed(&env_empty.ctx.home);
    let exe_b = env_empty.build_installer(
        "setup",
        b"MZ-STUB",
        "0.3.0-dev",
        &[(InstallLayout::AGENT_EXE, b"agent")],
    );
    install(&env_empty.ctx, &exe_b, &InstallOpts::default()).expect("install");
    uninstall(
        &env_empty.ctx,
        &UninstallOpts {
            delete_data: true,
            ..Default::default()
        },
    )
    .expect("uninstall delete_data");
    assert!(
        !env_empty.ctx.home.exists(),
        "empty home folder should be removed after deleting all listed entries"
    );
}

// 16. Delete-data guard refuses drive root, user profile, and %LOCALAPPDATA%.
#[test]
fn validate_delete_data_home_refuses_protected_roots() {
    let err = validate_delete_data_home(Path::new(r"C:\")).expect_err("drive root must fail");
    assert!(matches!(err, InstallError::Usage(_)));

    let err = validate_delete_data_home(Path::new(r"K:\")).expect_err("K:\\ must fail");
    assert!(matches!(err, InstallError::Usage(_)));

    if let Some(profile) = std::env::var_os("USERPROFILE") {
        let err = validate_delete_data_home(Path::new(&profile))
            .expect_err("USERPROFILE must fail");
        assert!(matches!(err, InstallError::Usage(_)));
    }

    if let Some(local_app) = std::env::var_os("LOCALAPPDATA") {
        let err = validate_delete_data_home(Path::new(&local_app))
            .expect_err("LOCALAPPDATA must fail");
        assert!(matches!(err, InstallError::Usage(_)));
    }
}

// 17. Progress: bytes reaches total once, steps arrive in order.
#[test]
fn progress_bytes_reach_total_once_and_steps_arrive_in_order() {
    struct Recorder {
        steps: Mutex<Vec<Step>>,
        bytes: Mutex<Vec<(u64, u64)>>,
    }
    impl Progress for Recorder {
        fn step(&self, step: Step) {
            self.steps.lock().unwrap().push(step);
        }
        fn bytes(&self, done: u64, total: u64) {
            self.bytes.lock().unwrap().push((done, total));
        }
        fn cancelled(&self) -> bool {
            false
        }
    }

    let env = TestEnv::new("progress");
    let exe = env.build_installer(
        "setup",
        b"MZ-STUB",
        "0.3.0-dev",
        &[
            ("a.bin", b"1234567890"),
            ("empty.bin", b""),
            (InstallLayout::AGENT_EXE, b"abcdefghijklmnop"),
        ],
    );
    let rec = Recorder {
        steps: Mutex::new(Vec::new()),
        bytes: Mutex::new(Vec::new()),
    };

    install_with_progress(
        &env.ctx,
        &exe,
        &InstallOpts::default(),
        available_disk_space,
        &rec,
    )
    .expect("install with progress");

    let steps = rec.steps.lock().unwrap().clone();
    assert_eq!(
        steps,
        vec![
            Step::Verifying,
            Step::Extracting,
            Step::Closing,
            Step::Swapping,
            Step::Registering,
            Step::Done,
        ]
    );

    let bytes = rec.bytes.lock().unwrap().clone();
    let expected_total = 10u64 + 16u64;
    assert!(!bytes.is_empty());
    let mut prev = 0u64;
    for &(done, total) in &bytes {
        assert_eq!(total, expected_total);
        assert!(done >= prev, "bytes must not decrease");
        assert!(done <= total, "bytes must not exceed total");
        prev = done;
    }
    let at_total = bytes.iter().filter(|&&(d, t)| d == t).count();
    assert_eq!(at_total, 1, "bytes must reach total exactly once");
    assert_eq!(bytes.last(), Some(&(expected_total, expected_total)));
}

// 18. License text reading from payload zip (present and missing).
#[test]
fn read_license_text_handles_present_and_missing() {
    let env = TestEnv::new("license");
    let with_lic = env.build_installer(
        "with-lic",
        b"MZ-STUB",
        "0.3.0-dev",
        &[
            (LICENSE_ENTRY, b"Line 1\nLine 2\r\nLine 3"),
            (InstallLayout::AGENT_EXE, b"agent"),
        ],
    );
    let text = read_license_text(&with_lic).expect("read_license_text");
    assert_eq!(text.as_deref(), Some("Line 1\r\nLine 2\r\nLine 3"));

    let without_lic = env.build_installer(
        "no-lic",
        b"MZ-STUB",
        "0.3.0-dev",
        &[(InstallLayout::AGENT_EXE, b"agent")],
    );
    assert_eq!(read_license_text(&without_lic).expect("no license"), None);
}

// 19. End-to-end through the built br-installer.exe: silent install, silent modify, silent uninstall
// with /deletedata and temp relaunch under --dev-root and --dev-reg-prefix.
#[test]
fn e2e_built_exe_silent_install_modify_and_uninstall() {
    let env = TestEnv::new("e2e");
    let bin_exe = Path::new(env!("CARGO_BIN_EXE_br-installer"));
    let zip_path = env.root.join("payload.zip");
    let built_setup = env.root.join("BetterRecall-Setup-e2e.exe");

    build_zip(
        &zip_path,
        "0.3.0-dev",
        &[
            (LICENSE_ENTRY, b"GNU AGPL v3"),
            (InstallLayout::AGENT_EXE, b"fake-agent"),
        ],
    );
    append_payload(bin_exe, &zip_path, &built_setup).expect("append_payload to built exe");

    // 1. Silent install with /norun /models=minimal against a closed local port (offline -> exit 0).
    let closed_port = closed_loopback_port();
    let test_cat_path = env.root.join("tiny-catalog.json");
    fs::write(&test_cat_path, tiny_catalog_json()).expect("write tiny catalog");
    let status = Command::new(&built_setup)
        .args([
            "/S",
            "/norun",
            "/models=minimal",
            "--dev-root",
            env.root.to_str().unwrap(),
            "--dev-reg-prefix",
            &env.reg_prefix,
            "--dev-models-catalog",
            test_cat_path.to_str().unwrap(),
            "--dev-models-url",
            &format!("http://127.0.0.1:{closed_port}/"),
        ])
        .creation_flags(CREATE_NO_WINDOW.0)
        .status()
        .expect("run built installer");
    assert_eq!(status.code(), Some(exit_code::OK));
    assert!(env.ctx.install_dir.join(InstallLayout::AGENT_EXE).exists());
    assert!(env.ctx.install_dir.join(SETUP_EXE).exists());
    assert_eq!(read_model_mode(&env.ctx), Some(ModelMode::Minimal));

    // Seed fake data in test home to confirm /deletedata cleans it.
    fs::write(env.ctx.home.join("settings.json"), b"{}").expect("write settings.json");

    // 2. Silent modify via installed BetterRecall-Setup.exe -> ModelMode = full (against closed local port).
    let installed_setup = env.ctx.install_dir.join(SETUP_EXE);
    let status = Command::new(&installed_setup)
        .args([
            "/modify",
            "/S",
            "/models=full",
            "--dev-root",
            env.root.to_str().unwrap(),
            "--dev-reg-prefix",
            &env.reg_prefix,
            "--dev-models-catalog",
            test_cat_path.to_str().unwrap(),
            "--dev-models-url",
            &format!("http://127.0.0.1:{closed_port}/"),
        ])
        .creation_flags(CREATE_NO_WINDOW.0)
        .status()
        .expect("run installed setup /modify");
    assert_eq!(status.code(), Some(exit_code::OK));
    assert_eq!(read_model_mode(&env.ctx), Some(ModelMode::Full));

    // 3. Silent uninstall with /deletedata via installed BetterRecall-Setup.exe (relaunches from <root>\temp).
    let status = Command::new(&installed_setup)
        .args([
            "/uninstall",
            "/S",
            "/deletedata",
            "--dev-root",
            env.root.to_str().unwrap(),
            "--dev-reg-prefix",
            &env.reg_prefix,
        ])
        .creation_flags(CREATE_NO_WINDOW.0)
        .status()
        .expect("run installed setup /uninstall");
    assert_eq!(status.code(), Some(exit_code::OK));

    // Poll up to 30 s for the relaunched temp copy to finish deleting install_dir and registry entries.
    let deadline = Instant::now() + Duration::from_secs(30);
    let uninstall_key = env.ctx.reg_key(UNINSTALL_SUBKEY);
    let app_key = env.ctx.reg_key(APP_SUBKEY);
    let run_key = env.ctx.reg_key(RUN_SUBKEY);
    loop {
        let install_gone = !env.ctx.install_dir.exists();
        let reg_gone = read_reg_sz(&uninstall_key, "DisplayName").unwrap().is_none()
            && read_reg_sz(&app_key, "InstallPath").unwrap().is_none()
            && read_reg_sz(&app_key, "ModelMode").unwrap().is_none()
            && read_reg_sz(&run_key, "BetterRecall").unwrap().is_none();
        let home_gone = !env.ctx.home.exists();
        if install_gone && reg_gone && home_gone {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for relaunched uninstall: install_gone={install_gone}, reg_gone={reg_gone}, home_gone={home_gone}"
        );
        thread::sleep(Duration::from_millis(100));
    }

    assert!(
        env.ctx.temp_dir.exists(),
        "relaunch should have used ctx.temp_dir (<root>\\temp)"
    );
}

// --- I3 local HTTP server & model download integration tests ---

fn sha256_hex(data: &[u8]) -> String {
    let digest = Sha256::digest(data);
    let mut s = String::with_capacity(64);
    for b in digest {
        let _ = write!(&mut s, "{b:02x}");
    }
    s
}

fn closed_loopback_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    listener.local_addr().unwrap().port()
}

struct TinyHttpServer {
    addr: String,
    requests: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl TinyHttpServer {
    fn start(files: HashMap<String, Vec<u8>>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let port = listener.local_addr().unwrap().port();
        let addr = format!("127.0.0.1:{port}");
        let files = Arc::new(files);
        let requests = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));

        let files_c = Arc::clone(&files);
        let requests_c = Arc::clone(&requests);
        let stop_c = Arc::clone(&stop);
        let handle = thread::spawn(move || {
            for incoming in listener.incoming() {
                if stop_c.load(AtomicOrdering::SeqCst) {
                    break;
                }
                if let Ok(mut stream) = incoming {
                    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                    let mut reader = BufReader::new(match stream.try_clone() {
                        Ok(s) => s,
                        Err(_) => continue,
                    });
                    let mut req_line = String::new();
                    if reader.read_line(&mut req_line).unwrap_or(0) == 0 {
                        continue;
                    }
                    let path = req_line
                        .split_whitespace()
                        .nth(1)
                        .unwrap_or("/")
                        .to_string();
                    requests_c.lock().unwrap().push(path.clone());
                    loop {
                        let mut line = String::new();
                        if reader.read_line(&mut line).unwrap_or(0) == 0 {
                            break;
                        }
                        if line.trim().is_empty() {
                            break;
                        }
                    }
                    if let Some(data) = files_c.get(&path) {
                        let _ = write!(
                            stream,
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            data.len()
                        );
                        let _ = stream.write_all(data);
                    } else {
                        let _ = write!(
                            stream,
                            "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        );
                    }
                }
            }
        });

        Self {
            addr,
            requests,
            stop,
            handle: Some(handle),
        }
    }

    fn base_url(&self) -> String {
        format!("http://{}/", self.addr)
    }

    fn requests(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }
}

impl Drop for TinyHttpServer {
    fn drop(&mut self) {
        self.stop.store(true, AtomicOrdering::SeqCst);
        let _ = TcpStream::connect(&self.addr);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn tiny_catalog_data() -> (Catalog, HashMap<String, Vec<u8>>) {
    let min_weights = b"minimal-weights-data".to_vec();
    let min_manifest = br#"{"id":"comp-min"}"#.to_vec();
    let full_weights = b"full-only-weights-data".to_vec();
    let full_manifest = br#"{"id":"comp-full"}"#.to_vec();

    let rev = "0123456789abcdef0123456789abcdef01234567";
    let cat = Catalog {
        repo: "test/models".to_string(),
        revision: rev.to_string(),
        base_url: format!("https://huggingface.co/test/models/resolve/{rev}/"),
        components: vec![
            Component {
                id: "comp-min".to_string(),
                sets: vec!["full".to_string(), "minimal".to_string()],
                bytes: (min_weights.len() + min_manifest.len()) as u64,
                files: vec![
                    CatalogFile {
                        path: "weights.bin".to_string(),
                        size: min_weights.len() as u64,
                        sha256: sha256_hex(&min_weights),
                    },
                    CatalogFile {
                        path: "manifest.json".to_string(),
                        size: min_manifest.len() as u64,
                        sha256: sha256_hex(&min_manifest),
                    },
                ],
            },
            Component {
                id: "comp-full".to_string(),
                sets: vec!["full".to_string()],
                bytes: (full_weights.len() + full_manifest.len()) as u64,
                files: vec![
                    CatalogFile {
                        path: "weights.bin".to_string(),
                        size: full_weights.len() as u64,
                        sha256: sha256_hex(&full_weights),
                    },
                    CatalogFile {
                        path: "manifest.json".to_string(),
                        size: full_manifest.len() as u64,
                        sha256: sha256_hex(&full_manifest),
                    },
                ],
            },
        ],
    };
    cat.validate().expect("tiny catalog validates");

    let mut files = HashMap::new();
    files.insert("/comp-min/weights.bin".to_string(), min_weights);
    files.insert("/comp-min/manifest.json".to_string(), min_manifest);
    files.insert("/comp-full/weights.bin".to_string(), full_weights);
    files.insert("/comp-full/manifest.json".to_string(), full_manifest);
    (cat, files)
}

fn tiny_catalog_json() -> String {
    let (cat, _) = tiny_catalog_data();
    serde_json::to_string(&cat).expect("serialize tiny catalog")
}

fn start_tiny_model_server() -> (TinyHttpServer, Catalog) {
    let (cat, files) = tiny_catalog_data();
    let server = TinyHttpServer::start(files);
    let cat = cat.with_base_url_for_test(server.base_url());
    (server, cat)
}

// 20. Install with minimal against local test server downloads models and records mode;
//     modify full -> minimal removes the full-only component; modify -> later keeps models.
#[test]
fn i3_install_minimal_and_modify_full_to_minimal_removes_full_only() {
    let env = TestEnv::new("i3-models");
    let exe = env.build_installer(
        "setup",
        b"MZ-STUB",
        "0.3.0-dev",
        &[(InstallLayout::AGENT_EXE, b"agent")],
    );
    let (_server, catalog) = start_tiny_model_server();

    let out = install(
        &env.ctx,
        &exe,
        &InstallOpts {
            allow_downgrade: false,
            model_mode: ModelMode::Minimal,
            skip_model_download: false,
            catalog: Some(catalog.clone()),
            integrity_key: None,
        },
    )
    .expect("install minimal");
    assert_eq!(
        out,
        Outcome::Installed {
            version: "0.3.0-dev".to_string(),
            models: ModelsResult::Complete,
        }
    );
    assert_eq!(read_model_mode(&env.ctx), Some(ModelMode::Minimal));

    let models_dir = resolve_models_dir(&env.ctx.home);
    assert!(models_dir.join("comp-min").join("manifest.json").exists());
    assert!(models_dir.join("comp-min").join("weights.bin").exists());
    assert!(!models_dir.join("comp-full").exists());

    // Modify to Full downloads comp-full.
    let out = modify_with_progress(
        &env.ctx,
        &ModifyOpts {
            model_mode: ModelMode::Full,
            catalog: Some(catalog.clone()),
            integrity_key: None,
        },
        &br_installer::NoProgress,
    )
    .expect("modify full");
    assert_eq!(
        out,
        Outcome::Modified {
            mode: ModelMode::Full,
            models: ModelsResult::Complete,
        }
    );
    assert_eq!(read_model_mode(&env.ctx), Some(ModelMode::Full));
    assert!(models_dir.join("comp-full").join("manifest.json").exists());

    // Modify from Full to Minimal removes comp-full and keeps comp-min.
    let out = modify_with_progress(
        &env.ctx,
        &ModifyOpts {
            model_mode: ModelMode::Minimal,
            catalog: Some(catalog.clone()),
            integrity_key: None,
        },
        &br_installer::NoProgress,
    )
    .expect("modify full -> minimal");
    assert_eq!(
        out,
        Outcome::Modified {
            mode: ModelMode::Minimal,
            models: ModelsResult::Complete,
        }
    );
    assert!(!models_dir.join("comp-full").exists());
    assert!(models_dir.join("comp-min").join("manifest.json").exists());

    // Modify to Later downloads nothing and removes nothing.
    let out = modify_with_progress(
        &env.ctx,
        &ModifyOpts {
            model_mode: ModelMode::Later,
            catalog: Some(catalog),
            integrity_key: None,
        },
        &br_installer::NoProgress,
    )
    .expect("modify later");
    assert_eq!(
        out,
        Outcome::Modified {
            mode: ModelMode::Later,
            models: ModelsResult::Skipped,
        }
    );
    assert!(models_dir.join("comp-min").join("manifest.json").exists());
}

// 21. Offline gives ModelsResult::Offline (silent e2e offline exit 0 is covered by e2e_built_exe_silent_install_modify_and_uninstall).
#[test]
fn i3_offline_returns_offline() {
    let env = TestEnv::new("i3-offline");
    let closed_port = closed_loopback_port();
    let (raw_cat, _) = tiny_catalog_data();
    let offline_cat = raw_cat.with_base_url_for_test(format!("http://127.0.0.1:{closed_port}/"));

    // Library path.
    let exe = env.build_installer(
        "setup",
        b"MZ-STUB",
        "0.3.0-dev",
        &[(InstallLayout::AGENT_EXE, b"agent")],
    );
    let out = install(
        &env.ctx,
        &exe,
        &InstallOpts {
            allow_downgrade: false,
            model_mode: ModelMode::Minimal,
            skip_model_download: false,
            catalog: Some(offline_cat),
            integrity_key: None,
        },
    )
    .expect("offline install must succeed");
    assert_eq!(
        out,
        Outcome::Installed {
            version: "0.3.0-dev".to_string(),
            models: ModelsResult::Offline,
        }
    );
    assert!(env.ctx.install_dir.join(InstallLayout::AGENT_EXE).exists());
    assert_eq!(read_model_mode(&env.ctx), Some(ModelMode::Minimal));
}

// 22. Silent e2e verify failure returns exit code 9 (InstallError::Models) with app still installed.
#[test]
fn i3_silent_e2e_verify_failure_exits_9_with_app_installed() {
    let env = TestEnv::new("i3-verify-fail");
    let (raw_cat, mut files) = tiny_catalog_data();
    // Corrupt weights.bin bytes on the server so SHA-256 verification fails.
    files.insert("/comp-min/weights.bin".to_string(), b"CORRUPTED-WEIGHTS!!!".to_vec());
    let server = TinyHttpServer::start(files);

    let bin_exe = Path::new(env!("CARGO_BIN_EXE_br-installer"));
    let zip_path = env.root.join("payload.zip");
    let built_setup = env.root.join("BetterRecall-Setup-badhash.exe");
    let cat_path = env.root.join("tiny-catalog.json");
    fs::write(&cat_path, serde_json::to_string(&raw_cat).unwrap()).expect("write catalog");
    build_zip(
        &zip_path,
        "0.3.0-dev",
        &[(InstallLayout::AGENT_EXE, b"fake-agent")],
    );
    append_payload(bin_exe, &zip_path, &built_setup).expect("append_payload");

    let status = Command::new(&built_setup)
        .args([
            "/S",
            "/norun",
            "/models=minimal",
            "--dev-root",
            env.root.to_str().unwrap(),
            "--dev-reg-prefix",
            &env.reg_prefix,
            "--dev-models-catalog",
            cat_path.to_str().unwrap(),
            "--dev-models-url",
            &server.base_url(),
        ])
        .creation_flags(CREATE_NO_WINDOW.0)
        .status()
        .expect("run built installer with bad hash");
    assert_eq!(status.code(), Some(exit_code::MODELS));
    assert!(env.ctx.install_dir.join(InstallLayout::AGENT_EXE).exists());
    assert_eq!(read_model_mode(&env.ctx), Some(ModelMode::Minimal));
}

// 23. Install-time free space check adds model bytes when a model set is chosen.
#[test]
fn i3_install_space_check_includes_model_bytes() {
    let env = TestEnv::new("i3-space");
    let exe = env.build_installer(
        "setup",
        b"MZ-STUB",
        "0.3.0-dev",
        &[(InstallLayout::AGENT_EXE, b"agent")],
    );
    let (mut cat, _) = tiny_catalog_data();
    cat.components[0].bytes = 100_000;

    // 50_000 bytes is plenty for the 5-byte payload, but not enough once model bytes are added.
    let err = install_with_space(
        &env.ctx,
        &exe,
        &InstallOpts {
            allow_downgrade: false,
            model_mode: ModelMode::Minimal,
            skip_model_download: false,
            catalog: Some(cat),
            integrity_key: None,
        },
        |_| Ok(50_000),
    )
    .expect_err("space check must include model bytes");
    assert!(matches!(err, InstallError::NotEnoughSpace { .. }));
}

// --- I7 integrity verification tests ---

fn gen_test_keypair() -> (String, String) {
    use p256::ecdsa::SigningKey;
    use p256::pkcs8::{EncodePrivateKey, EncodePublicKey, LineEnding};
    use rand_core::OsRng;

    let sk = SigningKey::random(&mut OsRng);
    let vk = sk.verifying_key();
    let priv_pem = sk.to_pkcs8_pem(LineEnding::LF).expect("priv pem").to_string();
    let pub_pem = vk.to_public_key_pem(LineEnding::LF).expect("pub pem");
    (priv_pem, pub_pem)
}

fn build_signed_installer(
    env: &TestEnv,
    name: &str,
    stub_bytes: &[u8],
    version: &str,
    files: &[(&str, &[u8])],
    tamper_agent_after_sign: bool,
) -> (PathBuf, String) {
    let (priv_pem, pub_pem) = gen_test_keypair();
    let layout_dir = env.root.join(format!("{name}-layout"));
    fs::create_dir_all(&layout_dir).expect("create layout");

    for (rel, data) in files {
        let full = layout_dir.join(rel);
        if let Some(p) = full.parent() {
            fs::create_dir_all(p).expect("create parent");
        }
        fs::write(&full, data).expect("write layout file");
    }
    // Include BetterRecall-Setup.exe in the manifest (it is copied after swap, not in the zip).
    fs::write(layout_dir.join(SETUP_EXE), stub_bytes).expect("write stub in layout");

    let manifest = br_integrity::build_manifest(&layout_dir, version).expect("build_manifest");
    let manifest_bytes = manifest.to_bytes().expect("manifest to_bytes");
    let sig_bytes = br_integrity::sign_manifest(&manifest_bytes, &priv_pem).expect("sign");

    let mut payload_entries: Vec<(String, Vec<u8>)> = files
        .iter()
        .map(|(rel, data)| ((*rel).to_string(), data.to_vec()))
        .collect();
    if tamper_agent_after_sign
        && let Some((_, data)) = payload_entries
            .iter_mut()
            .find(|(p, _)| p == InstallLayout::AGENT_EXE)
    {
        data[0] ^= 0xFF;
    }
    payload_entries.push((br_integrity::INTEGRITY_JSON.to_string(), manifest_bytes));
    payload_entries.push((br_integrity::INTEGRITY_SIG.to_string(), sig_bytes.to_vec()));

    let borrowed: Vec<(&str, &[u8])> = payload_entries
        .iter()
        .map(|(p, d)| (p.as_str(), d.as_slice()))
        .collect();

    let exe = env.build_installer(name, stub_bytes, version, &borrowed);
    (exe, pub_pem)
}

// 24. Signed installer passes staging + stub verification; tampered staged file fails with Integrity (exit 10)
//     and leaves no staging folder; tampered install refuses modify but allows uninstall.
#[test]
fn i7_integrity_staging_modify_and_uninstall_behaviour() {
    let env = TestEnv::new("i7-integrity");
    let stub = b"MZ-STUB-SIGNED-BYTES";

    // A. Tampered staged file fails with InstallError::Integrity (exit code 10) and cleans staging.
    let (bad_exe, bad_pub) = build_signed_installer(
        &env,
        "tampered",
        stub,
        "0.3.0-dev",
        &[
            (InstallLayout::AGENT_EXE, b"agent-signed"),
            (InstallLayout::WORKER_EXE, b"worker-signed"),
        ],
        true,
    );
    let err = install(
        &env.ctx,
        &bad_exe,
        &InstallOpts {
            integrity_key: Some(bad_pub),
            ..Default::default()
        },
    )
    .expect_err("tampered staged payload must fail integrity check");
    assert!(matches!(err, InstallError::Integrity(_)));
    assert_eq!(err.exit_code(), exit_code::INTEGRITY);
    assert!(!has_prefix_dir(&env.root, STAGING_PREFIX));
    assert!(!env.ctx.install_dir.exists());

    // B. Clean signed payload installs successfully (verifying staging and copied BetterRecall-Setup.exe).
    let (good_exe, pub_pem) = build_signed_installer(
        &env,
        "good",
        stub,
        "0.3.0-dev",
        &[
            (InstallLayout::AGENT_EXE, b"agent-signed"),
            (InstallLayout::WORKER_EXE, b"worker-signed"),
        ],
        false,
    );
    install(
        &env.ctx,
        &good_exe,
        &InstallOpts {
            integrity_key: Some(pub_pem.clone()),
            ..Default::default()
        },
    )
    .expect("clean signed install must pass");
    assert!(env.ctx.install_dir.join(SETUP_EXE).exists());

    // C. Tamper an installed file -> modify refuses with Integrity (exit 10), uninstall still succeeds.
    fs::write(
        env.ctx.install_dir.join(InstallLayout::AGENT_EXE),
        b"agent-tampered",
    )
    .expect("tamper installed agent");

    let mod_err = modify_with_progress(
        &env.ctx,
        &ModifyOpts {
            model_mode: ModelMode::Later,
            catalog: None,
            integrity_key: Some(pub_pem.clone()),
        },
        &br_installer::NoProgress,
    )
    .expect_err("modify must refuse damaged install");
    assert!(matches!(mod_err, InstallError::Integrity(_)));
    assert_eq!(mod_err.exit_code(), exit_code::INTEGRITY);

    let uninst = uninstall(
        &env.ctx,
        &UninstallOpts {
            delete_data: false,
            integrity_key: Some(pub_pem),
        },
    )
    .expect("uninstall must proceed on damaged install");
    assert_eq!(
        uninst,
        Outcome::Uninstalled {
            reboot_required: false
        }
    );
    assert!(!env.ctx.install_dir.exists());
}

// --- I5 silent upgrade model-mode preservation tests ---

// 25. Silent upgrade without /models= keeps Full in the registry and fetches nothing.
#[test]
fn i5_silent_upgrade_without_models_keeps_full_and_fetches_nothing() {
    let env = TestEnv::new("i5-silent-upgrade");
    let (raw_cat, files) = tiny_catalog_data();
    let server = TinyHttpServer::start(files);
    let cat_path = env.root.join("tiny-catalog.json");
    fs::write(&cat_path, serde_json::to_string(&raw_cat).unwrap()).expect("write catalog");

    // Seed an existing 0.3.0-dev install with ModelMode::Full recorded in the registry.
    let v1 = env.build_installer(
        "v1",
        b"MZ-STUB-V1",
        "0.3.0-dev",
        &[(InstallLayout::AGENT_EXE, b"agent-v1")],
    );
    install(
        &env.ctx,
        &v1,
        &InstallOpts {
            allow_downgrade: false,
            model_mode: ModelMode::Full,
            skip_model_download: true,
            catalog: None,
            integrity_key: None,
        },
    )
    .expect("seed v1");
    assert_eq!(read_model_mode(&env.ctx), Some(ModelMode::Full));

    // Upgrade silently to 0.3.0 without /models=, passing a dev catalog pointing at the local server.
    let v2 = env.build_installer(
        "v2",
        b"MZ-STUB-V2",
        "0.3.0",
        &[(InstallLayout::AGENT_EXE, b"agent-v2")],
    );
    let args = br_installer::cli::parse_args([
        "/S",
        "/norun",
        "--dev-root",
        env.root.to_str().unwrap(),
        "--dev-reg-prefix",
        &env.reg_prefix,
        "--dev-models-catalog",
        cat_path.to_str().unwrap(),
        "--dev-models-url",
        &server.base_url(),
    ])
    .expect("parse args");
    let out = br_installer::cli::dispatch_silent_for_exe(&env.ctx, &args, &v2)
        .expect("silent upgrade ok");
    assert_eq!(
        out,
        Outcome::Installed {
            version: "0.3.0".to_string(),
            models: ModelsResult::Skipped,
        }
    );
    assert_eq!(read_model_mode(&env.ctx), Some(ModelMode::Full));
    assert_eq!(
        fs::read(env.ctx.install_dir.join(InstallLayout::AGENT_EXE)).unwrap(),
        b"agent-v2"
    );
    assert!(
        server.requests().is_empty(),
        "silent upgrade must not fetch any models, got {:?}",
        server.requests()
    );
}

// 26. Fresh silent install without /models= records Later and fetches nothing.
#[test]
fn i5_fresh_silent_install_without_models_records_later() {
    let env = TestEnv::new("i5-silent-fresh");
    assert_eq!(read_model_mode(&env.ctx), None);

    let v1 = env.build_installer(
        "v1",
        b"MZ-STUB-V1",
        "0.3.0",
        &[(InstallLayout::AGENT_EXE, b"agent-v1")],
    );
    let args = br_installer::cli::parse_args([
        "/S",
        "/norun",
        "--dev-root",
        env.root.to_str().unwrap(),
        "--dev-reg-prefix",
        &env.reg_prefix,
    ])
    .expect("parse args");
    let out = br_installer::cli::dispatch_silent_for_exe(&env.ctx, &args, &v1)
        .expect("fresh silent install ok");
    assert_eq!(
        out,
        Outcome::Installed {
            version: "0.3.0".to_string(),
            models: ModelsResult::Skipped,
        }
    );
    assert_eq!(read_model_mode(&env.ctx), Some(ModelMode::Later));
}

// --- I6b relay, CLI shim, and launcher-era cleanup tests ---

// 27. Install places MCP relay and CLI shim; in-use relay is renamed aside and swept on next install;
//     uninstall removes both and prunes empty <home>\app\bin and <home>\app.
#[test]
fn i6b_relay_and_cli_shim_install_in_use_sweep_and_uninstall() {
    use windows::Win32::Storage::FileSystem::{FILE_SHARE_DELETE, FILE_SHARE_READ};

    let env = TestEnv::new("i6b-shims");
    let v1 = env.build_installer(
        "v1",
        b"MZ-STUB-V1",
        "0.3.0-dev.1",
        &[
            (InstallLayout::AGENT_EXE, b"agent-v1"),
            (InstallLayout::CLI_EXE, b"cli-v1"),
            (InstallLayout::MCP_EXE, b"mcp-v1"),
        ],
    );
    install(&env.ctx, &v1, &InstallOpts::default()).expect("install v1");

    let relay = br_installer::relay_path(&env.ctx.home);
    let shim = br_installer::cli_shim_path(&env.ctx.home);
    assert_eq!(fs::read(&relay).expect("relay v1"), b"mcp-v1");
    let expected_shim = format!(
        "@\"{}\" %*\r\n",
        env.ctx.install_dir.join(InstallLayout::CLI_EXE).display()
    );
    assert_eq!(fs::read(&shim).expect("shim v1"), expected_shim.as_bytes());

    // Hold the relay open with FILE_SHARE_READ | FILE_SHARE_DELETE (allows rename, blocks overwrite),
    // simulating a running MCP relay session during an upgrade.
    let share = (FILE_SHARE_READ | FILE_SHARE_DELETE).0;
    let lock = OpenOptions::new()
        .read(true)
        .share_mode(share)
        .open(&relay)
        .expect("lock relay");

    let v2 = env.build_installer(
        "v2",
        b"MZ-STUB-V2",
        "0.3.0-dev.2",
        &[
            (InstallLayout::AGENT_EXE, b"agent-v2"),
            (InstallLayout::CLI_EXE, b"cli-v2"),
            (InstallLayout::MCP_EXE, b"mcp-v2"),
        ],
    );
    install(&env.ctx, &v2, &InstallOpts::default()).expect("upgrade v2 with locked relay");
    assert_eq!(fs::read(&relay).expect("relay v2"), b"mcp-v2");
    let old1 = br_installer::bin_dir(&env.ctx.home).join("betterrecall-mcp.exe.old-1");
    assert!(old1.is_file(), "in-use relay must be renamed to .old-1");

    // Release the old relay handle; next install sweeps the stale .old-1 file.
    drop(lock);
    let v3 = env.build_installer(
        "v3",
        b"MZ-STUB-V3",
        "0.3.0-dev.3",
        &[
            (InstallLayout::AGENT_EXE, b"agent-v3"),
            (InstallLayout::CLI_EXE, b"cli-v3"),
            (InstallLayout::MCP_EXE, b"mcp-v3"),
        ],
    );
    install(&env.ctx, &v3, &InstallOpts::default()).expect("upgrade v3 sweeps .old-1");
    assert!(!old1.exists(), "stale .old-1 must be swept on next install");
    assert_eq!(fs::read(&relay).expect("relay v3"), b"mcp-v3");

    // Uninstall removes relay, CLI shim, and empty <home>\app\bin and <home>\app.
    uninstall(&env.ctx, &UninstallOpts::default()).expect("uninstall");
    assert!(!relay.exists());
    assert!(!shim.exists());
    assert!(!br_installer::bin_dir(&env.ctx.home).exists());
    assert!(!br_installer::app_dir(&env.ctx.home).exists());
}

// 28. Launcher-era cleanup removes version folders, bin\current, bin\BetterRecall.exe, and .tmp-*
//     while leaving non-matching folders and user data files untouched, and skipping in-use files.
#[test]
fn i6b_launcher_era_cleanup_removes_old_binaries_and_keeps_data() {
    let env = TestEnv::new("i6b-cleanup");
    let app = br_installer::app_dir(&env.ctx.home);
    let bin = br_installer::bin_dir(&env.ctx.home);
    let old_ver = app.join("0.1.0-0123456789ab");
    let locked_ver = app.join("0.2.0-fedcba987654");
    let tmp_dir = app.join(".tmp-leftover");
    let non_matching = app.join("custom-notes");

    fs::create_dir_all(&old_ver).unwrap();
    fs::create_dir_all(&locked_ver).unwrap();
    fs::create_dir_all(&tmp_dir).unwrap();
    fs::create_dir_all(&non_matching).unwrap();
    fs::create_dir_all(&bin).unwrap();

    fs::write(old_ver.join("BetterRecall.Agent.exe"), b"old").unwrap();
    let locked_file = locked_ver.join("locked.bin");
    fs::write(&locked_file, b"busy").unwrap();
    fs::write(tmp_dir.join("part.tmp"), b"tmp").unwrap();
    fs::write(non_matching.join("keep.txt"), b"keep").unwrap();
    fs::write(bin.join("current"), old_ver.to_string_lossy().as_bytes()).unwrap();
    fs::write(bin.join("BetterRecall.exe"), b"old-stub").unwrap();
    fs::write(bin.join(".tmp-staging"), b"tmp").unwrap();

    let settings_file = env.ctx.home.join("settings.json");
    fs::write(&settings_file, b"{}").unwrap();

    // Hold locked_file open with share_mode(0) so deletion fails and must be skipped, not abort install.
    let _lock = OpenOptions::new()
        .read(true)
        .share_mode(0)
        .open(&locked_file)
        .expect("lock file");

    let exe = env.build_installer(
        "setup",
        b"MZ-STUB",
        "0.3.0-dev",
        &[
            (InstallLayout::AGENT_EXE, b"agent"),
            (InstallLayout::CLI_EXE, b"cli"),
            (InstallLayout::MCP_EXE, b"mcp"),
        ],
    );
    install(&env.ctx, &exe, &InstallOpts::default()).expect("install with launcher cleanup");

    drop(_lock);
    assert!(!old_ver.exists(), "old version folder must be removed");
    assert!(!tmp_dir.exists(), ".tmp-* folder must be removed");
    assert!(!bin.join("current").exists(), "bin\\current must be removed");
    assert!(
        !bin.join("BetterRecall.exe").exists(),
        "bin\\BetterRecall.exe must be removed"
    );
    assert!(
        !bin.join(".tmp-staging").exists(),
        "bin\\.tmp-* must be removed"
    );
    assert!(
        locked_file.exists(),
        "in-use file must be skipped without failing install"
    );
    assert!(
        non_matching.join("keep.txt").is_file(),
        "non-matching folder must be untouched"
    );
    assert_eq!(
        fs::read(&settings_file).expect("settings.json untouched"),
        b"{}"
    );
}

// C1 (sweep 5): a launcher-era folder that a running program was started from is left alone
// (not half-deleted under it); it is removed on a later install once nothing runs from it.
#[test]
fn c1_launcher_cleanup_skips_a_folder_a_running_program_uses() {
    let env = TestEnv::new("c1-cleanup-running");
    let app = br_installer::app_dir(&env.ctx.home);
    let busy_ver = app.join("0.2.3-0123456789ab");
    fs::create_dir_all(&busy_ver).unwrap();
    fs::write(busy_ver.join("readme.txt"), b"old").unwrap();
    let exe = busy_ver.join("ping.exe");
    fs::copy(r"C:\Windows\System32\PING.EXE", &exe).expect("copy ping.exe");

    let images = vec![exe.clone()];
    assert!(br_installer::running::any_image_under(&images, &busy_ver));
    assert!(!br_installer::running::any_image_under(&images, &app.join("0.2.2-0123456789ab")));
    assert!(!br_installer::running::any_image_under(&[app.join("0.2.3-0123456789abX").join("a.exe")], &busy_ver));

    let mut child = Command::new(&exe)
        .args(["-n", "30", "127.0.0.1"])
        .creation_flags(CREATE_NO_WINDOW.0)
        .spawn()
        .expect("start copied ping.exe");
    br_installer::cleanup_launcher_installs(&env.ctx.home);
    let still_there = busy_ver.join("readme.txt").exists();
    let _ = child.kill();
    let _ = child.wait();
    assert!(still_there, "folder in use by a running program must be kept whole");

    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        br_installer::cleanup_launcher_installs(&env.ctx.home);
        if !busy_ver.exists() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "folder should be removed within 2 s once nothing runs from it: {}",
            busy_ver.display()
        );
        thread::sleep(Duration::from_millis(20));
    }
}

// B1: a Modify that downloads models does not hold SETUP_MUTEX during the download.
#[test]
fn b1_modify_does_not_hold_setup_mutex_during_download() {
    let env = TestEnv::new("b1-mutex");
    let exe = env.build_installer(
        "setup",
        b"MZ-STUB",
        "0.3.0-dev",
        &[(InstallLayout::AGENT_EXE, b"agent")],
    );
    let (_server, catalog) = start_tiny_model_server();

    // First install with Minimal models
    install(
        &env.ctx,
        &exe,
        &InstallOpts {
            allow_downgrade: false,
            model_mode: ModelMode::Minimal,
            skip_model_download: false,
            catalog: Some(catalog.clone()),
            integrity_key: None,
        },
    )
    .expect("install minimal");

    // Progress probe to check mutex state during DownloadingModels step
    struct Probe<'a> {
        home: &'a Path,
        step_seen: Arc<AtomicBool>,
        setup_mutex_free: Arc<AtomicBool>,
        setup_lock_held: Arc<AtomicBool>,
    }
    impl Progress for Probe<'_> {
        fn bytes(&self, _done: u64, _total: u64) {}
        fn cancelled(&self) -> bool {
            false
        }
        fn step(&self, s: Step) {
            if s == Step::DownloadingModels {
                self.step_seen.store(true, AtomicOrdering::SeqCst);

                // Verify SETUP_MUTEX is free (we can acquire it without ERROR_ALREADY_EXISTS)
                let mutex_name = PipeNames::setup_mutex_for(self.home);
                let wide_m: Vec<u16> = mutex_name.encode_utf16().chain(Some(0)).collect();
                let handle_m = unsafe { CreateMutexW(None, false, PCWSTR(wide_m.as_ptr())) };
                let last_m = unsafe { GetLastError() };
                if last_m != ERROR_ALREADY_EXISTS {
                    self.setup_mutex_free.store(true, AtomicOrdering::SeqCst);
                }
                if let Ok(h) = handle_m {
                    unsafe {
                        let _ = CloseHandle(h);
                    }
                }

                // Verify SETUP_LOCK is held (CreateMutexW reports ERROR_ALREADY_EXISTS)
                let lock_name = PipeNames::setup_lock_for(self.home);
                let wide_l: Vec<u16> = lock_name.encode_utf16().chain(Some(0)).collect();
                let handle_l = unsafe { CreateMutexW(None, false, PCWSTR(wide_l.as_ptr())) };
                let last_l = unsafe { GetLastError() };
                if last_l == ERROR_ALREADY_EXISTS {
                    self.setup_lock_held.store(true, AtomicOrdering::SeqCst);
                }
                if let Ok(h) = handle_l {
                    unsafe {
                        let _ = CloseHandle(h);
                    }
                }
            }
        }
    }

    let step_seen = Arc::new(AtomicBool::new(false));
    let setup_mutex_free = Arc::new(AtomicBool::new(false));
    let setup_lock_held = Arc::new(AtomicBool::new(false));
    let probe = Probe {
        home: &env.ctx.home,
        step_seen: Arc::clone(&step_seen),
        setup_mutex_free: Arc::clone(&setup_mutex_free),
        setup_lock_held: Arc::clone(&setup_lock_held),
    };

    let out = modify_with_progress(
        &env.ctx,
        &ModifyOpts {
            model_mode: ModelMode::Full,
            catalog: Some(catalog),
            integrity_key: None,
        },
        &probe,
    )
    .expect("modify to full");

    assert_eq!(
        out,
        Outcome::Modified {
            mode: ModelMode::Full,
            models: ModelsResult::Complete,
        }
    );
    assert!(step_seen.load(AtomicOrdering::SeqCst), "DownloadingModels step must be reached");
    assert!(
        setup_mutex_free.load(AtomicOrdering::SeqCst),
        "SETUP_MUTEX must NOT be held during model download"
    );
    assert!(
        setup_lock_held.load(AtomicOrdering::SeqCst),
        "SETUP_LOCK must be held during model download"
    );
}

// C3 (+F1): simulated in-use folder -> exit InUse, Uninstall key + Run value + shortcut still present, agent restart attempted.
#[test]
fn c3_uninstall_in_use_leaves_registry_and_shortcut_and_attempts_restart() {
    let env = TestEnv::new("c3-inuse");
    let ping_bytes = fs::read(r"C:\Windows\System32\PING.EXE").expect("read PING.EXE");
    let exe = env.build_installer(
        "setup",
        b"MZ-STUB",
        "0.3.0-dev",
        &[
            (InstallLayout::AGENT_EXE, ping_bytes.as_slice()),
            ("sibling.dll", b"locked-sibling"),
        ],
    );

    install(&env.ctx, &exe, &InstallOpts::default()).expect("install");
    assert!(env.ctx.install_dir.exists());
    assert!(shortcut_path(&env.ctx).exists());
    assert!(read_reg_sz(&env.ctx.reg_key(UNINSTALL_SUBKEY), "DisplayName").unwrap().is_some());
    assert!(read_reg_sz(&env.ctx.reg_key(RUN_SUBKEY), "BetterRecall").unwrap().is_some());

    // An agent "runs" for this data folder: its scoped quit event exists.
    let name: Vec<u16> = br_contracts::PipeNames::agent_quit_event_for(&env.ctx.home)
        .encode_utf16()
        .chain(Some(0))
        .collect();
    let ev = unsafe { windows::Win32::System::Threading::CreateEventW(None, true, false, PCWSTR(name.as_ptr())) }
        .expect("quit event");

    // Lock a file inside install_dir without FILE_SHARE_DELETE so folder rename fails with InUse.
    let lock_target = env.ctx.install_dir.join("sibling.dll");
    let _lock_file = OpenOptions::new()
        .read(true)
        .share_mode(1) // FILE_SHARE_READ only (no FILE_SHARE_DELETE)
        .open(&lock_target)
        .expect("lock sibling file");

    let res = uninstall_with_progress(&env.ctx, &UninstallOpts::default(), &br_installer::NoProgress);
    unsafe {
        let _ = CloseHandle(ev);
    }
    let err = match res {
        Err(e) => e,
        Ok(o) => panic!("uninstall must fail on locked folder, got {o:?}"),
    };
    assert!(matches!(err, InstallError::InUse { .. }), "expected InUse error, got {err:?}");

    // Registration, shortcut and Run value must remain untouched
    assert!(
        read_reg_sz(&env.ctx.reg_key(UNINSTALL_SUBKEY), "DisplayName").unwrap().is_some(),
        "Uninstall key must remain present"
    );
    assert!(
        read_reg_sz(&env.ctx.reg_key(RUN_SUBKEY), "BetterRecall").unwrap().is_some(),
        "Run value must remain present"
    );
    assert!(shortcut_path(&env.ctx).exists(), "shortcut must remain present");

    // Agent restart must have been attempted (logged)
    let log_path = env.ctx.home.join(r"logs\setup.log");
    assert!(log_path.is_file(), "setup.log must exist");
    let log_content = fs::read_to_string(&log_path).expect("read setup.log");
    assert!(
        log_content.contains("uninstall did not finish; restarting the agent that was running"),
        "log must contain restart attempt"
    );
}

// C5: stale-copy sweep (pattern match, keeps own file, ignores other names).
#[test]
fn c5_stale_uninstall_temp_copy_sweep() {
    let env = TestEnv::new("c5-sweep");
    let temp = env.root.join("temp");
    fs::create_dir_all(&temp).unwrap();

    let own_file = temp.join("BetterRecall-uninstall-456.exe");
    let stale_1 = temp.join("BetterRecall-uninstall-123.exe");
    let stale_2 = temp.join("BetterRecall-uninstall-789.exe");
    let other_file = temp.join("other-file.exe");
    let non_digit = temp.join("BetterRecall-uninstall-abc.exe");

    fs::write(&own_file, b"own").unwrap();
    fs::write(&stale_1, b"stale1").unwrap();
    fs::write(&stale_2, b"stale2").unwrap();
    fs::write(&other_file, b"other").unwrap();
    fs::write(&non_digit, b"nondigit").unwrap();

    // Verify pattern matching helper directly
    assert!(is_uninstall_temp_file_name("BetterRecall-uninstall-123.exe"));
    assert!(is_uninstall_temp_file_name("BetterRecall-uninstall-456.exe"));
    assert!(!is_uninstall_temp_file_name("other-file.exe"));
    assert!(!is_uninstall_temp_file_name("BetterRecall-uninstall-abc.exe"));
    assert!(!is_uninstall_temp_file_name("BetterRecall-uninstall-.exe"));

    sweep_stale_uninstall_temp(&temp, &own_file);

    assert!(!stale_1.exists(), "stale temp copy 1 must be deleted");
    assert!(!stale_2.exists(), "stale temp copy 2 must be deleted");
    assert!(own_file.exists(), "own temp file must be kept");
    assert!(other_file.exists(), "unrelated file must be kept");
    assert!(non_digit.exists(), "non-matching file must be kept");
}

// C6: Modify -> Minimal frees full-only components present on disk regardless of prev_mode.
#[test]
fn c6_modify_to_minimal_frees_full_models_when_prev_mode_not_full() {
    let env = TestEnv::new("c6-models");
    let exe = env.build_installer(
        "setup",
        b"MZ-STUB",
        "0.3.0-dev",
        &[(InstallLayout::AGENT_EXE, b"agent")],
    );
    let (_server, catalog) = start_tiny_model_server();

    // Install with Minimal
    install(
        &env.ctx,
        &exe,
        &InstallOpts {
            allow_downgrade: false,
            model_mode: ModelMode::Minimal,
            skip_model_download: false,
            catalog: Some(catalog.clone()),
            integrity_key: None,
        },
    )
    .expect("install minimal");

    let models_dir = resolve_models_dir(&env.ctx.home);
    assert!(models_dir.join("comp-min").join("manifest.json").exists());

    // Manually place full-only component on disk as if downloaded via Models page
    let comp_full_dir = models_dir.join("comp-full");
    fs::create_dir_all(&comp_full_dir).unwrap();
    fs::write(comp_full_dir.join("manifest.json"), b"{\"files\":[]}").unwrap();
    fs::write(comp_full_dir.join("weights.bin"), b"full-weights").unwrap();
    assert!(comp_full_dir.exists());

    // Stored mode is Minimal (NOT Full)
    assert_eq!(read_model_mode(&env.ctx), Some(ModelMode::Minimal));

    // Modify -> Minimal must free comp-full because it is on disk
    let out = modify_with_progress(
        &env.ctx,
        &ModifyOpts {
            model_mode: ModelMode::Minimal,
            catalog: Some(catalog),
            integrity_key: None,
        },
        &br_installer::NoProgress,
    )
    .expect("modify minimal");

    assert_eq!(
        out,
        Outcome::Modified {
            mode: ModelMode::Minimal,
            models: ModelsResult::Complete,
        }
    );
    assert!(!comp_full_dir.exists(), "full-only component must be freed on disk");
    assert!(models_dir.join("comp-min").join("manifest.json").exists());
}

// C6: Wizard on upgrade defaults the model choice to the stored ModelMode.
#[test]
fn c6_wizard_upgrade_defaults_to_stored_model_mode() {
    use br_installer::wizard::model::{WizardInit, WizardModel};

    // Upgrade with stored Full mode
    let init_full = WizardInit {
        action: Action::Install,
        existing_model_mode: Some(ModelMode::Full),
        cli_model_mode: None,
        cli_delete_data: false,
        cli_no_run: false,
        licence_found: true,
    };
    let model_full = WizardModel::new(init_full);
    assert_eq!(model_full.model_mode, ModelMode::Full, "stored Full mode must be default on upgrade");

    // Fresh install (no existing mode) falls back to Minimal
    let init_none = WizardInit {
        action: Action::Install,
        existing_model_mode: None,
        cli_model_mode: None,
        cli_delete_data: false,
        cli_no_run: false,
        licence_found: true,
    };
    let model_none = WizardModel::new(init_none);
    assert_eq!(model_none.model_mode, ModelMode::Minimal, "fresh install must default to Minimal");

    // Explicit CLI mode overrides stored mode
    let init_cli = WizardInit {
        action: Action::Install,
        existing_model_mode: Some(ModelMode::Full),
        cli_model_mode: Some(ModelMode::Later),
        cli_delete_data: false,
        cli_no_run: false,
        licence_found: true,
    };
    let model_cli = WizardModel::new(init_cli);
    assert_eq!(model_cli.model_mode, ModelMode::Later, "CLI flag overrides stored mode");
}

// C8: delete-data uninstall removes settings.json.bad, exports.pending, updates dir, and old relay copies.
#[test]
fn c8_delete_home_data_removes_bad_pending_updates_and_old_relay_files() {
    let env = TestEnv::new("c8-deldata");
    fs::create_dir_all(&env.ctx.home).unwrap();

    // Create the extra C8 state files
    let bad_settings = env.ctx.home.join("settings.json.bad");
    let exports_pending = env.ctx.home.join("exports.pending");
    fs::write(&bad_settings, b"bad-json").unwrap();
    fs::write(&exports_pending, b"pending-exports").unwrap();

    let updates_dir = DataPaths::updates_dir(&env.ctx.home);
    fs::create_dir_all(&updates_dir).unwrap();
    fs::write(updates_dir.join("update-temp.bin"), b"update-bytes").unwrap();

    let bin_dir = env.ctx.home.join("app").join("bin");
    fs::create_dir_all(&bin_dir).unwrap();
    let old_relay = bin_dir.join("betterrecall-mcp.exe.old-1");
    fs::write(&old_relay, b"old-relay").unwrap();

    // Standard items
    fs::write(env.ctx.home.join("settings.json"), b"{}").unwrap();
    fs::write(env.ctx.home.join("store.db"), b"store").unwrap();

    let exe = env.build_installer(
        "setup",
        b"MZ-STUB",
        "0.3.0-dev",
        &[(InstallLayout::AGENT_EXE, b"agent")],
    );
    install(&env.ctx, &exe, &InstallOpts::default()).expect("install");

    uninstall(
        &env.ctx,
        &UninstallOpts {
            delete_data: true,
            integrity_key: None,
        },
    )
    .expect("uninstall delete_data");

    assert!(!bad_settings.exists(), "settings.json.bad must be removed");
    assert!(!exports_pending.exists(), "exports.pending must be removed");
    assert!(!updates_dir.exists(), "cache\\updates must be removed");
    assert!(!old_relay.exists(), "app\\bin\\*.old-* must be removed");
    assert!(!env.ctx.home.join("settings.json").exists());
    assert!(!env.ctx.home.join("store.db").exists());
}

// C8: After unregister, delete Software\BetterRecall if it has no values and no subkeys left.
#[test]
fn c8_unregister_deletes_empty_app_key() {
    let env = TestEnv::new("c8-reg");
    let app_key = env.ctx.reg_key(APP_SUBKEY);

    // Case 1: only InstallPath and ModelMode values present -> key is removed
    write_reg_sz(&app_key, "InstallPath", "C:\\fake").unwrap();
    write_reg_sz(&app_key, "ModelMode", "minimal").unwrap();
    assert!(read_reg_sz(&app_key, "InstallPath").unwrap().is_some());

    unregister(&env.ctx).expect("unregister");
    assert_eq!(
        read_reg_sz(&app_key, "InstallPath").unwrap(),
        None,
        "empty app key must be removed"
    );

    // Case 2: key has an extra custom value -> key is preserved
    write_reg_sz(&app_key, "InstallPath", "C:\\fake").unwrap();
    write_reg_sz(&app_key, "UserSetting", "custom").unwrap();

    unregister(&env.ctx).expect("unregister");
    assert_eq!(read_reg_sz(&app_key, "InstallPath").unwrap(), None);
    assert_eq!(
        read_reg_sz(&app_key, "UserSetting").unwrap(),
        Some("custom".to_string()),
        "app key with extra values must be preserved"
    );
}

// F2: free-space check counts only components of the set not already installed and complete.
#[test]
fn f2_free_space_check_counts_only_uninstalled_model_components() {
    let env = TestEnv::new("f2-space");
    let (cat_data, files) = tiny_catalog_data();
    let server = TinyHttpServer::start(files.clone());
    let catalog = cat_data.with_base_url_for_test(server.base_url());

    let exe = env.build_installer(
        "setup",
        b"MZ-STUB",
        "0.3.0-dev",
        &[(InstallLayout::AGENT_EXE, b"agent")],
    );

    let models_dir = resolve_models_dir(&env.ctx.home);
    let comp_min_dir = models_dir.join("comp-min");
    fs::create_dir_all(&comp_min_dir).unwrap();

    // Create exact matching manifest so br_fetch::installed_state marks comp-min as Complete
    fs::write(comp_min_dir.join("manifest.json"), &files["/comp-min/manifest.json"]).unwrap();

    // Verify it is recognized as Complete by installed_state
    let states = br_fetch::installed_state(&models_dir, &catalog);
    assert_eq!(
        states.iter().find(|(id, _)| id == "comp-min").map(|(_, st)| *st),
        Some(br_fetch::ComponentState::Complete)
    );

    let opts = InstallOpts {
        allow_downgrade: false,
        model_mode: ModelMode::Full,
        skip_model_download: false,
        catalog: Some(catalog.clone()),
        integrity_key: None,
    };

    let res = install_with_space(&env.ctx, &exe, &opts, |_| {
        Ok(10) // refuse with NotEnoughSpace
    });
    let err = match res {
        Err(e) => e,
        Ok(o) => panic!("must fail space check, got {o:?}"),
    };

    if let InstallError::NotEnoughSpace { needed, .. } = err {
        // Full set has comp-min + comp-full.
        // Because comp-min is already complete, extra_model_bytes must only count comp-full!
        let full_comp = catalog.component("comp-full").unwrap();
        let app_manifest = br_installer::read_setup_manifest(&exe, &br_installer::read_trailer(&exe).unwrap()).unwrap();
        let expected_needed = app_manifest.bytes
            .saturating_add(app_manifest.bytes.div_ceil(10))
            .saturating_add(full_comp.bytes);
        assert_eq!(needed, expected_needed, "needed bytes must drop by installed comp-min size");
    } else {
        panic!("expected NotEnoughSpace, got {err:?}");
    }
}

// F4: per-user uninstall falls back to HKCU RunOnce cleanup when MoveFileEx fails.
#[test]
fn f4_uninstall_falls_back_to_runonce_cleanup_for_old_folder() {
    let env = TestEnv::new("f4-runonce");
    let exe = env.build_installer(
        "setup",
        b"MZ-STUB",
        "0.3.0-dev",
        &[(InstallLayout::AGENT_EXE, b"agent")],
    );
    install(&env.ctx, &exe, &InstallOpts::default()).expect("install");

    // Put an old install folder left behind with a locked file
    let parent = env.ctx.install_dir.parent().unwrap();
    let old_dir = parent.join(".BetterRecall.old-1234567890abcdef");
    fs::create_dir_all(&old_dir).unwrap();

    // Lock a file inside old_dir without FILE_SHARE_DELETE so remove_dir_all fails
    let locked_path = old_dir.join("locked.txt");
    fs::write(&locked_path, b"lock-me").unwrap();
    let _lock = OpenOptions::new()
        .read(true)
        .share_mode(1)
        .open(&locked_path)
        .expect("open locked file");

    let reboot_required = schedule_old_folder_cleanup(&env.ctx, &old_dir);
    assert!(reboot_required, "reboot_required must be true when RunOnce is scheduled");

    // Verify RunOnce value was written under dev registry prefix
    let runonce_key = env.ctx.reg_key(RUNONCE_SUBKEY);
    let val = read_reg_sz(&runonce_key, CLEANUP_VALUE_NAME).unwrap();
    assert!(val.is_some(), "RunOnce BetterRecallCleanup value must be written");
    let val_str = val.unwrap();
    assert!(
        val_str.contains("cmd.exe /d /c rmdir /s /q"),
        "command must be cmd.exe rmdir, got: {val_str}"
    );
    assert!(val_str.contains(".BetterRecall.old-1234567890abcdef"));
}

// N3: Exit 6 (NotEnoughSpace) logs needed and available bytes and the drive.
#[test]
fn n3_not_enough_space_logs_drive_needed_and_available() {
    let env = TestEnv::new("n3-space");
    let exe = env.build_installer(
        "setup",
        b"MZ-STUB",
        "0.3.0-dev",
        &[(InstallLayout::AGENT_EXE, b"agent")],
    );

    let res = install_with_space(&env.ctx, &exe, &InstallOpts::default(), |_| Ok(0));
    let err = match res {
        Err(e) => e,
        Ok(o) => panic!("must fail with space check, got {o:?}"),
    };
    assert!(matches!(err, InstallError::NotEnoughSpace { .. }));

    let log_path = env.ctx.home.join(r"logs\setup.log");
    assert!(log_path.is_file(), "setup.log must exist");
    let log = fs::read_to_string(&log_path).expect("read setup.log");
    assert!(log.contains("needed"), "log must mention needed bytes: {log}");
    assert!(log.contains("available"), "log must mention available bytes: {log}");
    assert!(log.contains("drive"), "log must mention drive: {log}");
}

// N4: Exit 7 (AlreadyRunning) logs `setup already running (mutex held)`.
#[test]
fn n4_already_running_logs_setup_already_running() {
    let env = TestEnv::new("n4-mutex");
    let exe = env.build_installer(
        "setup",
        b"MZ-STUB",
        "0.3.0-dev",
        &[(InstallLayout::AGENT_EXE, b"agent")],
    );

    // Pre-acquire SETUP_LOCK to simulate another installer running
    let lock_name = PipeNames::setup_lock_for(&env.ctx.home);
    let wide: Vec<u16> = lock_name.encode_utf16().chain(Some(0)).collect();
    let handle = unsafe { CreateMutexW(None, false, PCWSTR(wide.as_ptr())) }.expect("create lock mutex");

    let res = install(&env.ctx, &exe, &InstallOpts::default());
    let err = match res {
        Err(e) => e,
        Ok(o) => panic!("must fail AlreadyRunning, got {o:?}"),
    };
    assert!(matches!(err, InstallError::AlreadyRunning));

    unsafe {
        let _ = CloseHandle(handle);
    }

    let log_path = env.ctx.home.join(r"logs\setup.log");
    assert!(log_path.is_file(), "setup.log must exist");
    let log = fs::read_to_string(&log_path).expect("read setup.log");
    assert!(
        log.contains("setup already running (mutex held)"),
        "setup.log must contain expected message: {log}"
    );
}

// C1 installer half: writes DataPaths::updates_dir(home).join("last-install.json") on success and failure.
#[test]
fn c1_installer_writes_last_install_report() {
    let env = TestEnv::new("c1-report");
    let exe = env.build_installer(
        "setup",
        b"MZ-STUB",
        "0.3.0-dev",
        &[(InstallLayout::AGENT_EXE, b"agent")],
    );

    // Run successful install
    install(&env.ctx, &exe, &InstallOpts::default()).expect("install");

    let report_path = DataPaths::updates_dir(&env.ctx.home).join("last-install.json");
    assert!(report_path.is_file(), "last-install.json must exist after install");
    let content = fs::read_to_string(&report_path).expect("read last-install.json");
    let parsed: serde_json::Value = serde_json::from_str(&content).expect("parse json");
    assert_eq!(parsed["version"], "0.3.0-dev");
    assert_eq!(parsed["exitCode"], 0);
    assert!(parsed["utc"].as_str().is_some_and(|s| s.contains('T') && s.ends_with('Z')));

    // Run failed install (AlreadyRunning, exit 7)
    let lock_name = PipeNames::setup_lock_for(&env.ctx.home);
    let wide: Vec<u16> = lock_name.encode_utf16().chain(Some(0)).collect();
    let handle = unsafe { CreateMutexW(None, false, PCWSTR(wide.as_ptr())) }.expect("create lock mutex");

    let _ = install(&env.ctx, &exe, &InstallOpts::default());
    unsafe {
        let _ = CloseHandle(handle);
    }

    let content_fail = fs::read_to_string(&report_path).expect("read last-install.json on fail");
    let parsed_fail: serde_json::Value = serde_json::from_str(&content_fail).expect("parse fail json");
    assert_eq!(parsed_fail["version"], "0.3.0-dev");
    assert_eq!(parsed_fail["exitCode"], 7);
}
