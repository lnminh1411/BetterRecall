// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Offline release signing tool (`br-sign`, not shipped).
//!
//! Subcommands:
//! - `br-sign keygen --private <path> --public <path>`
//! - `br-sign manifest <layout dir> --key <private.pem> --version <v>`
//! - `br-sign verify <layout dir> --public <public.pem>`
//! - `br-sign update --installer <path> --version <v> --key <private.pem> --out <dir>`

use std::fmt::Write as _;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use br_integrity::{
    INTEGRITY_JSON, INTEGRITY_SIG, UPDATE_JSON, UPDATE_SIG, UpdateManifest, build_manifest,
    check_dir, sign_manifest, sign_update_manifest,
};
use p256::ecdsa::SigningKey;
use p256::pkcs8::{EncodePrivateKey, EncodePublicKey, LineEnding};
use rand_core::OsRng;
use sha2::{Digest, Sha256};

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
