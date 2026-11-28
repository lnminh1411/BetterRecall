// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Offline release signing tool (`br-sign`, not shipped).
//!
//! Subcommands:
//! - `br-sign keygen --private <path> --public <path>`
//! - `br-sign manifest <layout dir> --key <private.pem> --version <v>`
//! - `br-sign verify <layout dir> --public <public.pem>`
//! - `br-sign pack --stub <br-installer.exe> --layout <dir> --version <v> --out <file>`
//! - `br-sign update --installer <path> --version <v> --key <private.pem> --out <dir>`

use std::fmt::Write as _;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use br_installer::{SETUP_EXE, SETUP_JSON, SetupManifest, append_payload};
use br_integrity::{
    INTEGRITY_JSON, INTEGRITY_SIG, UPDATE_JSON, UPDATE_SIG, UpdateManifest, build_manifest,
    check_dir, sign_manifest, sign_update_manifest,
};
use p256::ecdsa::SigningKey;
use p256::pkcs8::{EncodePrivateKey, EncodePublicKey, LineEnding};
use rand_core::OsRng;
use sha2::{Digest, Sha256};
use zip::CompressionMethod;
use zip::write::{SimpleFileOptions, ZipWriter};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Err(msg) = run(&args) {
        eprintln!("br-sign: {msg}");
        std::process::exit(1);
    }
}

fn run(args: &[String]) -> Result<(), String> {
    let Some(cmd) = args.first().map(String::as_str) else {
        return Err(usage().to_string());
    };
    match cmd {
        "keygen" => cmd_keygen(&args[1..]),
        "manifest" => cmd_manifest(&args[1..]),
        "verify" => cmd_verify(&args[1..]),
        "pack" => cmd_pack(&args[1..]),
        "update" => cmd_update(&args[1..]),
        "-h" | "--help" | "help" => {
            println!("{}", usage());
            Ok(())
        }
        other => Err(format!("unknown subcommand '{other}'\n\n{}", usage())),
    }
}

fn usage() -> &'static str {
    "Usage:\n  \
     br-sign keygen --private <path> --public <path>\n  \
     br-sign manifest <layout dir> --key <private.pem> --version <v>\n  \
     br-sign verify <layout dir> --public <public.pem>\n  \
     br-sign pack --stub <br-installer.exe> --layout <dir> --version <v> --out <file>\n  \
     br-sign update --installer <path> --version <v> --key <private.pem> --out <dir>"
}

fn cmd_keygen(args: &[String]) -> Result<(), String> {
    let mut private_path: Option<PathBuf> = None;
    let mut public_path: Option<PathBuf> = None;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--private" if i + 1 < args.len() => {
                i += 1;
                private_path = Some(PathBuf::from(&args[i]));
            }
            "--public" if i + 1 < args.len() => {
                i += 1;
                public_path = Some(PathBuf::from(&args[i]));
            }
            other => return Err(format!("unexpected argument '{other}'")),
        }
        i += 1;
    }

    let priv_p = private_path.ok_or("keygen requires --private <path>")?;
    let pub_p = public_path.ok_or("keygen requires --public <path>")?;

    if priv_p.exists() {
        return Err(format!(
            "refusing to overwrite existing private key '{}'",
            priv_p.display()
        ));
    }
    if pub_p.exists() {
        return Err(format!(
            "refusing to overwrite existing public key '{}'",
            pub_p.display()
        ));
    }

    let signing_key = SigningKey::random(&mut OsRng);
    let verifying_key = signing_key.verifying_key();

    let priv_pem = signing_key
        .to_pkcs8_pem(LineEnding::LF)
        .map_err(|e| format!("encoding private key PEM: {e}"))?;
    let pub_pem = verifying_key
        .to_public_key_pem(LineEnding::LF)
        .map_err(|e| format!("encoding public key PEM: {e}"))?;
    let spki_der = verifying_key
        .to_public_key_der()
        .map_err(|e| format!("encoding public key DER: {e}"))?;

    write_new_file(&priv_p, priv_pem.as_bytes())?;
    write_new_file(&pub_p, pub_pem.as_bytes())?;

    let digest = Sha256::digest(spki_der.as_bytes());
    let mut fp = String::with_capacity(64);
    for b in digest {
        let _ = write!(fp, "{b:02x}");
    }
    println!("{fp}");
    Ok(())
}

fn cmd_manifest(args: &[String]) -> Result<(), String> {
    let mut layout_dir: Option<PathBuf> = None;
    let mut key_path: Option<PathBuf> = None;
    let mut version: Option<String> = None;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--key" if i + 1 < args.len() => {
                i += 1;
                key_path = Some(PathBuf::from(&args[i]));
            }
            "--version" if i + 1 < args.len() => {
                i += 1;
                version = Some(args[i].clone());
            }
            other if !other.starts_with('-') && layout_dir.is_none() => {
                layout_dir = Some(PathBuf::from(other));
            }
            other => return Err(format!("unexpected argument '{other}'")),
        }
        i += 1;
    }

    let dir = layout_dir.ok_or("manifest requires <layout dir>")?;
    let key_p = key_path.ok_or("manifest requires --key <private.pem>")?;
    let ver = version.ok_or("manifest requires --version <v>")?;

    let priv_pem = fs::read_to_string(&key_p)
        .map_err(|e| format!("reading private key '{}': {e}", key_p.display()))?;
    let manifest = build_manifest(&dir, &ver).map_err(|e| format!("building manifest: {e}"))?;
    let manifest_bytes = manifest
        .to_bytes()
        .map_err(|e| format!("serializing manifest: {e}"))?;
    let sig = sign_manifest(&manifest_bytes, &priv_pem)
        .map_err(|e| format!("signing manifest: {e}"))?;

    let json_path = dir.join(INTEGRITY_JSON);
    let sig_path = dir.join(INTEGRITY_SIG);
    fs::write(&json_path, &manifest_bytes)
        .map_err(|e| format!("writing '{}': {e}", json_path.display()))?;
    fs::write(&sig_path, sig).map_err(|e| format!("writing '{}': {e}", sig_path.display()))?;

    println!(
        "signed {} files (version {}) -> {}, {}",
        manifest.files.len(),
        manifest.version,
        json_path.display(),
        sig_path.display()
    );
    Ok(())
}

fn cmd_verify(args: &[String]) -> Result<(), String> {
    let mut layout_dir: Option<PathBuf> = None;
    let mut pub_path: Option<PathBuf> = None;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--public" if i + 1 < args.len() => {
                i += 1;
                pub_path = Some(PathBuf::from(&args[i]));
            }
            other if !other.starts_with('-') && layout_dir.is_none() => {
                layout_dir = Some(PathBuf::from(other));
            }
            other => return Err(format!("unexpected argument '{other}'")),
        }
        i += 1;
    }

    let dir = layout_dir.ok_or("verify requires <layout dir>")?;
    let pub_p = pub_path.ok_or("verify requires --public <public.pem>")?;

    let pub_pem = fs::read_to_string(&pub_p)
        .map_err(|e| format!("reading public key '{}': {e}", pub_p.display()))?;
    check_dir(&dir, &pub_pem).map_err(|e| format!("integrity check failed: {e}"))?;
    println!("ok: {}", dir.display());
    Ok(())
}

fn cmd_pack(args: &[String]) -> Result<(), String> {
    let mut stub_path: Option<PathBuf> = None;
    let mut layout_dir: Option<PathBuf> = None;
    let mut version: Option<String> = None;
    let mut out_file: Option<PathBuf> = None;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--stub" if i + 1 < args.len() => {
                i += 1;
                stub_path = Some(PathBuf::from(&args[i]));
            }
            "--layout" if i + 1 < args.len() => {
                i += 1;
                layout_dir = Some(PathBuf::from(&args[i]));
            }
            "--version" if i + 1 < args.len() => {
                i += 1;
                version = Some(args[i].clone());
            }
            "--out" if i + 1 < args.len() => {
                i += 1;
                out_file = Some(PathBuf::from(&args[i]));
            }
            other => return Err(format!("unexpected argument '{other}'")),
        }
        i += 1;
    }

    let stub = stub_path.ok_or("pack requires --stub <br-installer.exe>")?;
    let layout = layout_dir.ok_or("pack requires --layout <dir>")?;
    let ver = version.ok_or("pack requires --version <v>")?;
    let out = out_file.ok_or("pack requires --out <file>")?;

    if !stub.is_file() {
        return Err(format!("stub executable not found: '{}'", stub.display()));
    }
    if !layout.is_dir() {
        return Err(format!("layout directory not found: '{}'", layout.display()));
    }
    if ver.trim().is_empty() {
        return Err("version must not be empty".to_string());
    }

    if let Some(parent) = out.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)
            .map_err(|e| format!("creating parent dir '{}': {e}", parent.display()))?;
    }

    let mut rel_files: Vec<(String, PathBuf, u64)> = Vec::new();
    collect_layout_files(&layout, "", &mut rel_files)?;
    rel_files.sort_by_key(|a| a.0.to_ascii_lowercase());

    let total_bytes: u64 = rel_files
        .iter()
        .map(|(_, _, sz)| *sz)
        .fold(0u64, u64::saturating_add);
    let manifest = SetupManifest {
        version: ver,
        files: rel_files.len() as u64,
        bytes: total_bytes,
    };

    let tmp_zip = out.with_extension(format!("payload-{}.zip.tmp", std::process::id()));
    let pack_res = (|| -> Result<(), String> {
        let zip_file = File::create(&tmp_zip)
            .map_err(|e| format!("creating temp zip '{}': {e}", tmp_zip.display()))?;
        let mut writer = ZipWriter::new(zip_file);
        let opts = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);

        let manifest_json = serde_json::to_string(&manifest)
            .map_err(|e| format!("serializing {SETUP_JSON}: {e}"))?;
        writer
            .start_file(SETUP_JSON, opts)
            .map_err(|e| format!("starting {SETUP_JSON}: {e}"))?;
        writer
            .write_all(manifest_json.as_bytes())
            .map_err(|e| format!("writing {SETUP_JSON}: {e}"))?;

        for (entry_name, full_path, _) in &rel_files {
            writer
                .start_file(entry_name, opts)
                .map_err(|e| format!("starting zip entry '{entry_name}': {e}"))?;
            let mut src = File::open(full_path)
                .map_err(|e| format!("opening '{}': {e}", full_path.display()))?;
            io::copy(&mut src, &mut writer)
                .map_err(|e| format!("writing zip entry '{entry_name}': {e}"))?;
        }
        writer
            .finish()
            .map_err(|e| format!("finishing zip '{}': {e}", tmp_zip.display()))?;

        let trailer = append_payload(&stub, &tmp_zip, &out)
            .map_err(|e| format!("appending payload to '{}': {e}", out.display()))?;
        println!(
            "packed {} files ({} bytes uncompressed, {} bytes compressed, sha256 {}) -> {}",
            manifest.files,
            manifest.bytes,
            trailer.length,
            trailer.sha256_hex(),
            out.display()
        );
        Ok(())
    })();

    let _ = fs::remove_file(&tmp_zip);
    pack_res
}

fn collect_layout_files(
    root: &Path,
    prefix: &str,
    out: &mut Vec<(String, PathBuf, u64)>,
) -> Result<(), String> {
    let dir = if prefix.is_empty() {
        root.to_path_buf()
    } else {
        root.join(prefix)
    };
    let entries =
        fs::read_dir(&dir).map_err(|e| format!("reading directory '{}': {e}", dir.display()))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("reading entry in '{}': {e}", dir.display()))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let rel = if prefix.is_empty() {
            name
        } else {
            format!("{prefix}/{name}")
        };
        let path = entry.path();
        let meta = entry
            .metadata()
            .map_err(|e| format!("reading metadata for '{}': {e}", path.display()))?;
        if meta.is_dir() {
            collect_layout_files(root, &rel, out)?;
        } else if meta.is_file() {
            // BetterRecall-Setup.exe is written from the stub bytes after swap; setup.json is generated.
            if rel.eq_ignore_ascii_case(SETUP_EXE) || rel.eq_ignore_ascii_case(SETUP_JSON) {
                continue;
            }
            out.push((rel.replace('\\', "/"), path, meta.len()));
        }
    }
    Ok(())
}

fn cmd_update(args: &[String]) -> Result<(), String> {
    let mut installer_path: Option<PathBuf> = None;
    let mut version: Option<String> = None;
    let mut key_path: Option<PathBuf> = None;
    let mut out_dir: Option<PathBuf> = None;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--installer" if i + 1 < args.len() => {
                i += 1;
                installer_path = Some(PathBuf::from(&args[i]));
            }
            "--version" if i + 1 < args.len() => {
                i += 1;
                version = Some(args[i].clone());
            }
            "--key" if i + 1 < args.len() => {
                i += 1;
                key_path = Some(PathBuf::from(&args[i]));
            }
            "--out" if i + 1 < args.len() => {
                i += 1;
                out_dir = Some(PathBuf::from(&args[i]));
            }
            other => return Err(format!("unexpected argument '{other}'")),
        }
        i += 1;
    }

    let inst = installer_path.ok_or("update requires --installer <path>")?;
    let ver = version.ok_or("update requires --version <v>")?;
    let key_p = key_path.ok_or("update requires --key <private.pem>")?;
    let out = out_dir.ok_or("update requires --out <dir>")?;

    let priv_pem = fs::read_to_string(&key_p)
        .map_err(|e| format!("reading private key '{}': {e}", key_p.display()))?;
    let manifest = UpdateManifest::from_installer(&inst, &ver)
        .map_err(|e| format!("building update manifest: {e}"))?;
    let (bytes, sig) = sign_update_manifest(&manifest, &priv_pem)
        .map_err(|e| format!("signing update manifest: {e}"))?;

    fs::create_dir_all(&out).map_err(|e| format!("creating '{}': {e}", out.display()))?;
    let json_path = out.join(UPDATE_JSON);
    let sig_path = out.join(UPDATE_SIG);
    fs::write(&json_path, &bytes).map_err(|e| format!("writing '{}': {e}", json_path.display()))?;
    fs::write(&sig_path, sig).map_err(|e| format!("writing '{}': {e}", sig_path.display()))?;

    println!(
        "signed update {} ({}, {} bytes) -> {}, {}",
        manifest.version,
        manifest.installer,
        manifest.size,
        json_path.display(),
        sig_path.display()
    );
    Ok(())
}

fn write_new_file(path: &Path, data: &[u8]) -> Result<(), String> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)
            .map_err(|e| format!("creating parent dir '{}': {e}", parent.display()))?;
    }
    let mut f = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|e| format!("creating '{}': {e}", path.display()))?;
    f.write_all(data)
        .map_err(|e| format!("writing '{}': {e}", path.display()))
}
