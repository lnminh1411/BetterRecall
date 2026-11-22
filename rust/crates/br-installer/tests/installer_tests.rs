// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Integration tests for `br-installer` (stage I1).
//! Every test runs in a unique subfolder under `BR_TEST_TMP` and a unique
//! `HKCU\Software\BetterRecallTests\<unique>` prefix, cleaned up on drop.

use std::cmp::Ordering;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::windows::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::time::Instant;

use br_contracts::InstallLayout;
use br_installer::{
    APP_SUBKEY, Action, Ctx, DISPLAY_NAME, InstallError, InstallOpts, OLD_PREFIX, Outcome,
    PUBLISHER, ProcInfo, RUN_SUBKEY, SETUP_EXE, SETUP_JSON, STAGING_PREFIX, SetupManifest,
    TRAILER_LEN, UNINSTALL_SUBKEY, URL_INFO_ABOUT, UninstallOpts, append_payload, close_running,
    compare_versions, create_shortcut, delete_key_tree, extract_to_staging,
    extract_to_staging_with_space, install, install_with_space, modify, parse_args,
    pick_processes_to_end, read_install_marker, read_reg_dword, read_reg_sz, read_shortcut_target,
    read_trailer, register, remove_shortcut, shortcut_path, stub_len, swap_into_place, uninstall,
    unregister, verify, write_reg_sz,
};
use windows::Win32::System::Registry::{HKEY_CURRENT_USER, RegDeleteKeyW};
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
            version: "0.3.0-dev".to_string()
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
            version: "0.3.0".to_string()
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

    let outcome = install(&env.ctx, &older, &InstallOpts { allow_downgrade: true })
        .expect("allow_downgrade permits downgrade");
    assert_eq!(
        outcome,
        Outcome::Installed {
            version: "0.3.0".to_string()
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
#[should_panic]
fn ctx_for_test_panics_on_bad_reg_prefix() {
    let env = TestEnv::new("bad-prefix");
    let _ = Ctx::for_test(&env.root, r"Software\BetterRecall");
}

#[test]
#[should_panic]
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
fn modify_returns_nothing_to_do_in_i1() {
    let env = TestEnv::new("modify");
    assert_eq!(modify(&env.ctx).expect("modify"), Outcome::NothingToDo);
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
}
