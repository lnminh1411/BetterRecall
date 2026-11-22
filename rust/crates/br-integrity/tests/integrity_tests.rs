// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Unit and cross-language golden fixture tests for `br-integrity` (stage I7).

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use br_integrity::{
    CheckOutcome, INTEGRITY_JSON, INTEGRITY_SIG, INSTALL_MARKER, IntegrityError,
    IntegrityManifest, MANIFEST_FORMAT, ManifestFile, UpdateManifest, build_manifest, check_dir,
    check_dir_with_exempt, check_install, check_single_file, sign_manifest, sign_update_manifest,
    verify_signature, verify_update_manifest,
};
use p256::ecdsa::SigningKey;
use p256::pkcs8::{EncodePrivateKey, EncodePublicKey, LineEnding};
use rand_core::OsRng;

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(label: &str) -> Self {
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let base = std::env::var_os("BR_TEST_TMP")
            .filter(|v| !v.to_string_lossy().trim().is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(r"K:\scratch\br-rust\test-tmp"));
        let path = base.join(format!("i7-{label}-{pid}-{id}"));
        fs::create_dir_all(&path).expect("create test temp dir");
        Self { path }
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn gen_keypair() -> (String, String) {
    let sk = SigningKey::random(&mut OsRng);
    let vk = sk.verifying_key();
    let priv_pem = sk.to_pkcs8_pem(LineEnding::LF).expect("priv pem").to_string();
    let pub_pem = vk.to_public_key_pem(LineEnding::LF).expect("pub pem");
    (priv_pem, pub_pem)
}

fn create_signed_layout(dir: &Path, version: &str, files: &[(&str, &[u8])]) -> (String, String) {
    for (rel, data) in files {
        let full = dir.join(rel);
        if let Some(parent) = full.parent() {
            fs::create_dir_all(parent).expect("create parent");
        }
        fs::write(&full, data).expect("write file");
    }
    let (priv_pem, pub_pem) = gen_keypair();
    let manifest = build_manifest(dir, version).expect("build_manifest");
    let bytes = manifest.to_bytes().expect("to_bytes");
    let sig = sign_manifest(&bytes, &priv_pem).expect("sign_manifest");
    fs::write(dir.join(INTEGRITY_JSON), &bytes).expect("write integrity.json");
    fs::write(dir.join(INTEGRITY_SIG), sig).expect("write integrity.sig");
    (priv_pem, pub_pem)
}

fn golden_fixture_root() -> PathBuf {
    // CARGO_MANIFEST_DIR = <repo>/rust/crates/br-integrity
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("..")
        .join("tests")
        .join("fixtures")
        .join("integrity")
}

fn copy_dir_recursive(src: &Path, dst: &Path) {
    fs::create_dir_all(dst).expect("create dst");
    for entry in fs::read_dir(src).expect("read_dir") {
        let entry = entry.expect("entry");
        let dst_path = dst.join(entry.file_name());
        if entry.file_type().expect("ft").is_dir() {
            copy_dir_recursive(&entry.path(), &dst_path);
        } else {
            fs::copy(entry.path(), dst_path).expect("copy");
        }
    }
}

// 1. Manifest round trip, deterministic bytes, strict validation.
#[test]
fn manifest_round_trip_deterministic_and_strict_validation() {
    let valid_hash = "a".repeat(64);
    let m = IntegrityManifest {
        format: MANIFEST_FORMAT,
        version: "0.3.0-dev".to_string(),
        files: vec![
            ManifestFile {
                path: "BetterRecall.Agent.exe".to_string(),
                size: 123,
                sha256: valid_hash.clone(),
            },
            ManifestFile {
                path: r"worker\BetterRecall.Worker.exe".to_string(),
                size: 456,
                sha256: valid_hash.clone(),
            },
        ],
    };

    let bytes1 = m.to_bytes().expect("to_bytes 1");
    let bytes2 = m.to_bytes().expect("to_bytes 2");
    assert_eq!(bytes1, bytes2);
    assert!(!bytes1.starts_with(&[0xEF, 0xBB, 0xBF]));
    assert!(!bytes1.contains(&b'\r'));
    assert!(bytes1.ends_with(b"\n"));

    let parsed = IntegrityManifest::from_bytes(&bytes1).expect("round trip");
    assert_eq!(parsed, m);

    // BOM rejected.
    let mut with_bom = vec![0xEF, 0xBB, 0xBF];
    with_bom.extend_from_slice(&bytes1);
    assert!(matches!(
        IntegrityManifest::from_bytes(&with_bom),
        Err(IntegrityError::BadManifest(_))
    ));

    // Bad paths: .., drive, absolute, forward slash, excluded name.
    for bad_path in [
        r"..\evil.dll",
        r"sub\..\evil.dll",
        r"C:\Windows\System32\cmd.exe",
        r"\leading.dll",
        "forward/slash.dll",
        "integrity.json",
        "integrity.sig",
        ".install",
        "",
    ] {
        let bad = IntegrityManifest {
            format: MANIFEST_FORMAT,
            version: "0.3.0".to_string(),
            files: vec![ManifestFile {
                path: bad_path.to_string(),
                size: 1,
                sha256: valid_hash.clone(),
            }],
        };
        assert!(
            matches!(bad.validate(), Err(IntegrityError::BadManifest(_))),
            "expected BadManifest for path '{bad_path}'"
        );
    }

    // Bad hashes: wrong length, uppercase, non-hex.
    for bad_hash in ["abc", &"A".repeat(64), &"g".repeat(64)] {
        let bad = IntegrityManifest {
            format: MANIFEST_FORMAT,
            version: "0.3.0".to_string(),
            files: vec![ManifestFile {
                path: "ok.exe".to_string(),
                size: 1,
                sha256: bad_hash.to_string(),
            }],
        };
        assert!(
            matches!(bad.validate(), Err(IntegrityError::BadManifest(_))),
            "expected BadManifest for hash '{bad_hash}'"
        );
    }

    // Unsorted entries rejected.
    let unsorted = IntegrityManifest {
        format: MANIFEST_FORMAT,
        version: "0.3.0".to_string(),
        files: vec![
            ManifestFile {
                path: "z.exe".to_string(),
                size: 1,
                sha256: valid_hash.clone(),
            },
            ManifestFile {
                path: "a.exe".to_string(),
                size: 1,
                sha256: valid_hash.clone(),
            },
        ],
    };
    assert!(matches!(
        unsorted.validate(),
        Err(IntegrityError::BadManifest(_))
    ));

    // Case-insensitive duplicates rejected.
    let duplicate = IntegrityManifest {
        format: MANIFEST_FORMAT,
        version: "0.3.0".to_string(),
        files: vec![
            ManifestFile {
                path: "App.exe".to_string(),
                size: 1,
                sha256: valid_hash.clone(),
            },
            ManifestFile {
                path: "app.exe".to_string(),
                size: 1,
                sha256: valid_hash,
            },
        ],
    };
    assert!(matches!(
        duplicate.validate(),
        Err(IntegrityError::BadManifest(_))
    ));
}

// 2. check_dir ok, flipped byte, missing file, unlisted root & subfolder, .install allowed, wrong key, flipped sig.
#[test]
fn check_dir_detects_all_tampering_and_allows_install_marker() {
    let tmp = TempDir::new("check-dir");
    let (_, pub_pem) = create_signed_layout(
        &tmp.path,
        "0.3.0-dev",
        &[
            ("BetterRecall.Agent.exe", b"agent-bytes-1234"),
            (r"worker\BetterRecall.Worker.exe", b"worker-bytes-5678"),
            (r"ui\BetterRecall.UI.exe", b"ui-bytes-9012"),
        ],
    );

    // Clean layout passes.
    check_dir(&tmp.path, &pub_pem).expect("clean layout ok");

    // `.install` marker is allowed without being listed.
    fs::write(tmp.path.join(INSTALL_MARKER), "0.3.0-dev\nabc\n3\n").expect("write .install");
    check_dir(&tmp.path, &pub_pem).expect(".install allowed");

    // Flipped byte -> Changed(path).
    let agent_path = tmp.path.join("BetterRecall.Agent.exe");
    let orig_agent = fs::read(&agent_path).expect("read agent");
    let mut flipped = orig_agent.clone();
    flipped[0] ^= 0x01;
    fs::write(&agent_path, &flipped).expect("write flipped");
    let err = check_dir(&tmp.path, &pub_pem).expect_err("flipped byte must fail");
    assert!(matches!(err, IntegrityError::Changed(ref p) if p == "BetterRecall.Agent.exe"));
    fs::write(&agent_path, &orig_agent).expect("restore agent");

    // Removed file -> Missing(path).
    let worker_path = tmp.path.join(r"worker\BetterRecall.Worker.exe");
    let orig_worker = fs::read(&worker_path).expect("read worker");
    fs::remove_file(&worker_path).expect("remove worker");
    let err = check_dir(&tmp.path, &pub_pem).expect_err("missing file must fail");
    assert!(
        matches!(err, IntegrityError::Missing(ref p) if p == r"worker\BetterRecall.Worker.exe")
    );
    // Exempting the missing file lets staging check pass; check_single_file detects it missing.
    check_dir_with_exempt(&tmp.path, &pub_pem, &[r"worker\BetterRecall.Worker.exe"])
        .expect("exempt missing file ok");
    assert!(matches!(
        check_single_file(&tmp.path, &pub_pem, r"worker\BetterRecall.Worker.exe"),
        Err(IntegrityError::Missing(_))
    ));
    fs::write(&worker_path, &orig_worker).expect("restore worker");
    check_single_file(&tmp.path, &pub_pem, r"worker\BetterRecall.Worker.exe")
        .expect("check_single_file ok");

    // Unlisted file at root -> Unlisted("evil.dll").
    let evil_root = tmp.path.join("evil.dll");
    fs::write(&evil_root, b"planted").expect("write evil.dll");
    let err = check_dir(&tmp.path, &pub_pem).expect_err("unlisted root file must fail");
    assert!(matches!(err, IntegrityError::Unlisted(ref p) if p == "evil.dll"));
    fs::remove_file(&evil_root).expect("remove evil.dll");

    // Unlisted file in subfolder -> Unlisted("worker\\evil.dll").
    let evil_sub = tmp.path.join(r"worker\evil.dll");
    fs::write(&evil_sub, b"planted-sub").expect("write sub evil.dll");
    let err = check_dir(&tmp.path, &pub_pem).expect_err("unlisted sub file must fail");
    assert!(matches!(err, IntegrityError::Unlisted(ref p) if p == r"worker\evil.dll"));
    fs::remove_file(&evil_sub).expect("remove sub evil.dll");

    // Wrong public key -> BadSignature.
    let (_, other_pub) = gen_keypair();
    let err = check_dir(&tmp.path, &other_pub).expect_err("wrong key must fail");
    assert!(matches!(err, IntegrityError::BadSignature(_)));

    // Flipped signature byte -> BadSignature.
    let sig_path = tmp.path.join(INTEGRITY_SIG);
    let orig_sig = fs::read(&sig_path).expect("read sig");
    let mut bad_sig = orig_sig.clone();
    bad_sig[10] ^= 0x80;
    fs::write(&sig_path, &bad_sig).expect("write bad sig");
    let err = check_dir(&tmp.path, &pub_pem).expect_err("flipped sig byte must fail");
    assert!(matches!(err, IntegrityError::BadSignature(_)));
    fs::write(&sig_path, &orig_sig).expect("restore sig");
}

// 3. check_install returns Skipped in the test build (no BR_RELEASE_PUBKEY).
#[test]
fn check_install_skips_without_release_key() {
    let tmp = TempDir::new("skip");
    assert!(matches!(check_install(&tmp.path), CheckOutcome::Skipped));
}

// 4. Update manifest sign and verify; tampered size or version fails.
#[test]
fn update_manifest_sign_verify_and_tamper_detection() {
    let tmp = TempDir::new("update");
    let installer_path = tmp.path.join("BetterRecall-Setup-0.3.1.exe");
    fs::write(&installer_path, b"MZ-FAKE-INSTALLER-PAYLOAD").expect("write installer");

    let (priv_pem, pub_pem) = gen_keypair();
    let manifest = UpdateManifest::from_installer(&installer_path, "0.3.1").expect("from_installer");
    let (bytes, sig) = sign_update_manifest(&manifest, &priv_pem).expect("sign_update");

    let verified = verify_update_manifest(&bytes, &sig, &pub_pem).expect("verify ok");
    assert_eq!(verified, manifest);

    // Tampered version fails signature verification.
    let tampered_ver = String::from_utf8(bytes.clone())
        .unwrap()
        .replace("0.3.1", "0.3.2");
    assert!(matches!(
        verify_update_manifest(tampered_ver.as_bytes(), &sig, &pub_pem),
        Err(IntegrityError::BadSignature(_))
    ));

    // Tampered size fails signature verification.
    let tampered_size = String::from_utf8(bytes)
        .unwrap()
        .replace(&manifest.size.to_string(), &(manifest.size + 1).to_string());
    assert!(matches!(
        verify_update_manifest(tampered_size.as_bytes(), &sig, &pub_pem),
        Err(IntegrityError::BadSignature(_))
    ));
}

// 6. Golden cross-language fixture verification and tamper detection.
#[test]
fn golden_fixture_verifies_and_detects_modified_copy() {
    let fixture_root = golden_fixture_root();
    let pub_pem = fs::read_to_string(fixture_root.join("public.pem")).expect("read golden pubkey");
    let layout_dir = fixture_root.join("layout");

    check_dir(&layout_dir, &pub_pem).expect("golden fixture must verify");

    // Copy to temp dir and tamper one byte -> Changed.
    let tmp = TempDir::new("golden-copy");
    copy_dir_recursive(&layout_dir, &tmp.path);
    check_dir(&tmp.path, &pub_pem).expect("copied golden fixture must verify");

    let target = tmp.path.join(r"sub\nested.txt");
    let mut data = fs::read(&target).expect("read nested");
    data[0] ^= 0x01;
    fs::write(&target, &data).expect("tamper nested");
    let err = check_dir(&tmp.path, &pub_pem).expect_err("tampered golden copy must fail");
    assert!(matches!(err, IntegrityError::Changed(ref p) if p == r"sub\nested.txt"));
}

/// Maintainer helper to regenerate `tests\fixtures\integrity\` (ignored in normal test runs).
#[test]
#[ignore]
fn regenerate_golden_fixture() {
    let fixture_root = golden_fixture_root();
    let layout_dir = fixture_root.join("layout");
    if layout_dir.exists() {
        fs::remove_dir_all(&layout_dir).expect("clean layout");
    }
    fs::create_dir_all(layout_dir.join("sub")).expect("create layout");

    let (_, pub_pem) = create_signed_layout(
        &layout_dir,
        "0.3.0-fixture",
        &[
            ("app.txt", b"BetterRecall golden fixture file 1\n"),
            ("data.bin", b"\x00\x01\x02\x03\x04\x05\x06\x07betterrecall"),
            (r"sub\nested.txt", b"Nested file inside subfolder\n"),
        ],
    );
    fs::write(fixture_root.join("public.pem"), &pub_pem).expect("write public.pem");

    // Sanity check immediately.
    let manifest_bytes = fs::read(layout_dir.join(INTEGRITY_JSON)).expect("read manifest");
    let sig_bytes = fs::read(layout_dir.join(INTEGRITY_SIG)).expect("read sig");
    verify_signature(&manifest_bytes, &sig_bytes, &pub_pem).expect("verify sig");
    check_dir(&layout_dir, &pub_pem).expect("verify layout");
}
