// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

use std::env;
use std::fs;
use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-env-changed=BR_RELEASE_PUBKEY");
    println!("cargo::rustc-check-cfg=cfg(br_release)");

    let Ok(raw_path) = env::var("BR_RELEASE_PUBKEY") else {
        return;
    };
    let trimmed = raw_path.trim();
    if trimmed.is_empty() {
        return;
    }

    let src = PathBuf::from(trimmed);
    println!("cargo:rerun-if-changed={}", src.display());

    let bytes = fs::read(&src).unwrap_or_else(|e| {
        panic!(
            "BR_RELEASE_PUBKEY points to '{}', which could not be read: {e}",
            src.display()
        )
    });
    if bytes.is_empty() {
        panic!("BR_RELEASE_PUBKEY file '{}' is empty", src.display());
    }

    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR not set"));
    let dst = out_dir.join("release_pubkey.pem");
    fs::write(&dst, &bytes).unwrap_or_else(|e| {
        panic!("failed to write embedded key to '{}': {e}", dst.display())
    });

    println!("cargo:rustc-cfg=br_release");
}
