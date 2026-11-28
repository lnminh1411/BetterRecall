// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Tests for Authenticode-signed installer trailer discovery, verification, and stub restoration.

use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};

use br_contracts::InstallLayout;
use br_installer::install::copy_stub_exe;
use br_installer::payload::pe_security_dir;
use br_installer::{
    Ctx, InstallError, InstallOpts, ModelsResult, Outcome, SETUP_EXE, SETUP_JSON, SetupManifest,
    TRAILER_LEN, append_payload, delete_key_tree, install, read_trailer, stub_len, verify,
};
use windows::Win32::System::Registry::{HKEY_CURRENT_USER, RegDeleteKeyW};
use windows::core::PCWSTR;
use zip::CompressionMethod;
use zip::write::{SimpleFileOptions, ZipWriter};

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

struct SignTestEnv {
    root: PathBuf,
    reg_prefix: String,
    ctx: Ctx,
}

impl SignTestEnv {
    fn new(label: &str) -> Self {
        let id = NEXT_ID.fetch_add(1, AtomicOrdering::Relaxed);
        let pid = std::process::id();
        let unique = format!("authsign-{label}-{pid}-{id}");
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
}

impl Drop for SignTestEnv {
    fn drop(&mut self) {
        let _ = delete_key_tree(&self.reg_prefix);
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

/// Builds a minimal valid PE32+ (or PE32) image with 16 data directories, all zeroed
/// CheckSum and security directory, plus `extra_tail` deterministic bytes at the end.
fn make_minimal_pe_stub(is_pe32_plus: bool, extra_tail: usize) -> Vec<u8> {
    let e_lfanew: usize = 0x40;
    let opt_size: u16 = if is_pe32_plus { 240 } else { 224 };
    let base_len = e_lfanew + 24 + usize::from(opt_size) + 0x40;
    let mut buf = vec![0u8; base_len + extra_tail];

    buf[0..2].copy_from_slice(b"MZ");
    buf[0x3C..0x40].copy_from_slice(&(e_lfanew as u32).to_le_bytes());
    buf[e_lfanew..e_lfanew + 4].copy_from_slice(b"PE\0\0");

    // COFF file header (20 bytes at e_lfanew + 4): Machine, NumberOfSections, ..., SizeOfOptionalHeader.
    let machine: u16 = if is_pe32_plus { 0x8664 } else { 0x014C };
    buf[e_lfanew + 4..e_lfanew + 6].copy_from_slice(&machine.to_le_bytes());
    buf[e_lfanew + 20..e_lfanew + 22].copy_from_slice(&opt_size.to_le_bytes());

    let opt_off = e_lfanew + 24;
    let magic: u16 = if is_pe32_plus { 0x020B } else { 0x010B };
    buf[opt_off..opt_off + 2].copy_from_slice(&magic.to_le_bytes());

    let num_rva_off = opt_off + if is_pe32_plus { 108 } else { 92 };
    buf[num_rva_off..num_rva_off + 4].copy_from_slice(&16u32.to_le_bytes());

    // Fill the trailing bytes with a deterministic non-zero pattern.
    for (i, b) in buf[e_lfanew + 24 + usize::from(opt_size)..].iter_mut().enumerate() {
        *b = ((i % 251) + 1) as u8;
    }
    buf
}

/// Simulates Authenticode signing on `unsigned_installer`:
/// - pads with 0..7 zero bytes to 8-byte alignment
/// - appends `cert_len` fake certificate-table bytes
/// - writes a non-zero CheckSum and populates data directory 4 (`IMAGE_DIRECTORY_ENTRY_SECURITY`).
///
/// Returns `(signed_bytes, pad_len, cert_offset)`.
fn simulate_authenticode_sign(
    unsigned_installer: &[u8],
    cert_len: usize,
    is_pe32_plus: bool,
) -> (Vec<u8>, usize, usize) {
    let pad_len = (8 - (unsigned_installer.len() % 8)) % 8;
    let cert_offset = unsigned_installer.len() + pad_len;

    let mut signed = Vec::with_capacity(cert_offset + cert_len);
    signed.extend_from_slice(unsigned_installer);
    signed.resize(cert_offset, 0u8);
    signed.extend((0..cert_len).map(|i| (0xA0u8).wrapping_add((i % 16) as u8)));

    let e_lfanew = u32::from_le_bytes(signed[0x3C..0x40].try_into().unwrap()) as usize;
    let opt_off = e_lfanew + 24;
    let checksum_off = opt_off + 64;
    let sec_dir_off = opt_off + if is_pe32_plus { 112 } else { 96 } + 4 * 8;

    signed[checksum_off..checksum_off + 4].copy_from_slice(&0xDEAD_BEEFu32.to_le_bytes());
    signed[sec_dir_off..sec_dir_off + 4].copy_from_slice(&(cert_offset as u32).to_le_bytes());
    signed[sec_dir_off + 4..sec_dir_off + 8].copy_from_slice(&(cert_len as u32).to_le_bytes());

    (signed, pad_len, cert_offset)
}

#[test]
fn signed_installer_trailer_and_verify_work_for_all_alignment_paddings() {
    let env = SignTestEnv::new("pad0-7");
    let zip_path = env.root.join("payload.zip");
    build_zip(
        &zip_path,
        "0.4.0-dev",
        &[(InstallLayout::AGENT_EXE, b"agent-binary-bytes")],
    );
    let zip_len = fs::metadata(&zip_path).expect("zip meta").len() as usize;

    for target_pad in 0..=7usize {
        // Choose stub length so (stub_len + zip_len + TRAILER_LEN + target_pad) % 8 == 0.
        let base_stub = make_minimal_pe_stub(true, 0);
        let rem = (base_stub.len() + zip_len + TRAILER_LEN as usize + target_pad) % 8;
        let extra = (8 - rem) % 8;
        let stub = make_minimal_pe_stub(true, extra);

        let stub_path = env.root.join(format!("stub-{target_pad}.exe"));
        let unsigned_path = env.root.join(format!("unsigned-{target_pad}.exe"));
        let signed_path = env.root.join(format!("signed-{target_pad}.exe"));
        fs::write(&stub_path, &stub).expect("write stub");
        let expected_trailer =
            append_payload(&stub_path, &zip_path, &unsigned_path).expect("append_payload");

        let unsigned_bytes = fs::read(&unsigned_path).expect("read unsigned");
        let (signed_bytes, actual_pad, cert_offset) =
            simulate_authenticode_sign(&unsigned_bytes, 128, true);
        assert_eq!(actual_pad, target_pad, "alignment padding mismatch");
        assert_eq!(cert_offset % 8, 0, "certificate table must be 8-byte aligned");
        fs::write(&signed_path, &signed_bytes).expect("write signed");

        let mut f = File::open(&signed_path).expect("open signed");
        assert_eq!(
            pe_security_dir(&mut f).expect("pe_security_dir"),
            Some((cert_offset as u64, 128))
        );

        let found_trailer = read_trailer(&signed_path).expect("read_trailer on signed");
        assert_eq!(
            found_trailer, expected_trailer,
            "trailer mismatch for padding={target_pad}"
        );
        verify(&signed_path, &found_trailer).expect("verify on signed");
    }
}

#[test]
fn signed_installer_rejects_security_dir_not_ending_at_eof() {
    let env = SignTestEnv::new("bad-secdir");
    let zip_path = env.root.join("payload.zip");
    build_zip(&zip_path, "0.4.0-dev", &[("file.txt", b"hello")]);

    let stub = make_minimal_pe_stub(true, 0);
    let stub_path = env.root.join("stub.exe");
    let unsigned_path = env.root.join("unsigned.exe");
    fs::write(&stub_path, &stub).expect("write stub");
    let trailer = append_payload(&stub_path, &zip_path, &unsigned_path).expect("append_payload");

    let unsigned_bytes = fs::read(&unsigned_path).expect("read unsigned");
    let (mut trailing_bytes, _, _) = simulate_authenticode_sign(&unsigned_bytes, 64, true);
    // Append extra bytes after the certificate table -> offset + size < file_len.
    trailing_bytes.extend_from_slice(b"EXTRA");
    let trailing_path = env.root.join("trailing-after-cert.exe");
    fs::write(&trailing_path, &trailing_bytes).expect("write trailing");

    let err = read_trailer(&trailing_path).expect_err("trailing bytes after cert must fail");
    assert!(
        matches!(&err, InstallError::Payload(msg) if msg.contains("certificate table is not at the end of the file")),
        "unexpected error: {err:?}"
    );
    let verify_err = verify(&trailing_path, &trailer).expect_err("verify must also reject");
    assert!(matches!(verify_err, InstallError::Payload(_)));

    // Truncate inside the certificate table -> offset + size > file_len.
    let (mut truncated_bytes, _, _) = simulate_authenticode_sign(&unsigned_bytes, 64, true);
    truncated_bytes.truncate(truncated_bytes.len() - 8);
    let truncated_path = env.root.join("truncated-cert.exe");
    fs::write(&truncated_path, &truncated_bytes).expect("write truncated");

    let err = read_trailer(&truncated_path).expect_err("truncated cert table must fail");
    assert!(
        matches!(&err, InstallError::Payload(msg) if msg.contains("certificate table is not at the end of the file")),
        "unexpected error: {err:?}"
    );
}

#[test]
fn signed_installer_rejects_non_zero_alignment_padding() {
    let env = SignTestEnv::new("nonzero-pad");
    let zip_path = env.root.join("payload.zip");
    build_zip(&zip_path, "0.4.0-dev", &[("file.txt", b"hello")]);
    let zip_len = fs::metadata(&zip_path).expect("zip meta").len() as usize;

    // Force target_pad == 3 so there is padding to corrupt.
    let base_stub = make_minimal_pe_stub(true, 0);
    let rem = (base_stub.len() + zip_len + TRAILER_LEN as usize + 3) % 8;
    let stub = make_minimal_pe_stub(true, (8 - rem) % 8);
    let stub_path = env.root.join("stub.exe");
    let unsigned_path = env.root.join("unsigned.exe");
    fs::write(&stub_path, &stub).expect("write stub");
    append_payload(&stub_path, &zip_path, &unsigned_path).expect("append_payload");

    let unsigned_bytes = fs::read(&unsigned_path).expect("read unsigned");
    let (mut signed_bytes, pad_len, cert_offset) =
        simulate_authenticode_sign(&unsigned_bytes, 64, true);
    assert_eq!(pad_len, 3);
    signed_bytes[cert_offset - 1] = 0xFF;

    let bad_path = env.root.join("bad-pad.exe");
    fs::write(&bad_path, &signed_bytes).expect("write bad pad");
    let err = read_trailer(&bad_path).expect_err("non-zero alignment padding must fail");
    assert!(matches!(err, InstallError::Payload(_)));
}

#[test]
fn copy_stub_exe_and_install_restore_exact_unsigned_stub_bytes() {
    for is_pe32_plus in [true, false] {
        let label = if is_pe32_plus { "pe32plus" } else { "pe32" };
        let env = SignTestEnv::new(label);
        let stub = make_minimal_pe_stub(is_pe32_plus, 3);
        let stub_path = env.root.join("stub.exe");
        let zip_path = env.root.join("payload.zip");
        let unsigned_path = env.root.join("unsigned.exe");
        let signed_path = env.root.join("signed.exe");

        fs::write(&stub_path, &stub).expect("write stub");
        build_zip(
            &zip_path,
            "0.4.0-dev",
            &[(InstallLayout::AGENT_EXE, b"agent-payload")],
        );
        append_payload(&stub_path, &zip_path, &unsigned_path).expect("append_payload");

        let unsigned_bytes = fs::read(&unsigned_path).expect("read unsigned");
        let (signed_bytes, _, _) = simulate_authenticode_sign(&unsigned_bytes, 96, is_pe32_plus);
        fs::write(&signed_path, &signed_bytes).expect("write signed");

        // Signed prefix differs from unsigned stub (CheckSum and security dir are non-zero).
        assert_ne!(&signed_bytes[..stub.len()], stub.as_slice());

        // Direct copy_stub_exe restores exact unsigned stub bytes.
        let direct_dir = env.root.join("direct");
        fs::create_dir_all(&direct_dir).expect("create direct dir");
        let trailer = read_trailer(&signed_path).expect("read_trailer");
        copy_stub_exe(&signed_path, stub_len(&trailer), &direct_dir).expect("copy_stub_exe");
        let copied_direct = fs::read(direct_dir.join(SETUP_EXE)).expect("read copied stub");
        assert_eq!(copied_direct, stub);

        // Full install also places byte-identical unsigned stub.
        let outcome = install(&env.ctx, &signed_path, &InstallOpts::default()).expect("install");
        assert_eq!(
            outcome,
            Outcome::Installed {
                version: "0.4.0-dev".to_string(),
                models: ModelsResult::Skipped,
            }
        );
        let installed_stub = fs::read(env.ctx.install_dir.join(SETUP_EXE)).expect("read installed");
        assert_eq!(installed_stub, stub);
    }
}

#[test]
fn unsigned_pe_and_non_pe_installers_still_work() {
    let env = SignTestEnv::new("unsigned");
    let zip_path = env.root.join("payload.zip");
    build_zip(&zip_path, "0.4.0-dev", &[(InstallLayout::AGENT_EXE, b"agent")]);

    // 1. Unsigned valid PE32+ stub.
    let pe_stub = make_minimal_pe_stub(true, 5);
    let pe_stub_path = env.root.join("pe-stub.exe");
    let pe_installer = env.root.join("pe-unsigned.exe");
    fs::write(&pe_stub_path, &pe_stub).expect("write pe stub");
    let pe_expected = append_payload(&pe_stub_path, &zip_path, &pe_installer).expect("append");

    let mut f = File::open(&pe_installer).expect("open pe installer");
    assert_eq!(pe_security_dir(&mut f).expect("pe_security_dir"), None);
    let pe_found = read_trailer(&pe_installer).expect("read_trailer");
    assert_eq!(pe_found, pe_expected);
    verify(&pe_installer, &pe_found).expect("verify");

    let pe_out = env.root.join("pe-out");
    fs::create_dir_all(&pe_out).expect("mkdir");
    copy_stub_exe(&pe_installer, stub_len(&pe_found), &pe_out).expect("copy_stub_exe");
    assert_eq!(fs::read(pe_out.join(SETUP_EXE)).expect("read"), pe_stub);

    // 2. Non-PE plain bytes stub (unit-test fixture style).
    let plain_stub = b"NOT-A-PE-STUB-0123456789";
    let plain_stub_path = env.root.join("plain-stub.bin");
    let plain_installer = env.root.join("plain-unsigned.exe");
    fs::write(&plain_stub_path, plain_stub).expect("write plain stub");
    let plain_expected =
        append_payload(&plain_stub_path, &zip_path, &plain_installer).expect("append");

    let mut f2 = File::open(&plain_installer).expect("open plain installer");
    assert_eq!(pe_security_dir(&mut f2).expect("pe_security_dir"), None);
    let plain_found = read_trailer(&plain_installer).expect("read_trailer");
    assert_eq!(plain_found, plain_expected);
    verify(&plain_installer, &plain_found).expect("verify");

    let plain_out = env.root.join("plain-out");
    fs::create_dir_all(&plain_out).expect("mkdir");
    copy_stub_exe(&plain_installer, stub_len(&plain_found), &plain_out).expect("copy_stub_exe");
    assert_eq!(
        fs::read(plain_out.join(SETUP_EXE)).expect("read"),
        plain_stub
    );
}
