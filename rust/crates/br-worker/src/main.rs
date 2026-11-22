// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
#![windows_subsystem = "windows"]

fn main() {
    let root = std::env::current_exe()
        .ok()
        .and_then(|p| {
            p.parent()
                .and_then(|d| d.parent())
                .map(std::path::Path::to_path_buf)
        })
        .unwrap_or_else(|| std::path::PathBuf::from(".."));
    br_integrity::enforce_or_exit(&root, br_integrity::ProgramKind::Headless);
    std::process::exit(br_worker::run());
}
