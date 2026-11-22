// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Integration tests for `br-sign pack`.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use br_contracts::InstallLayout;
use br_installer::{
    Ctx, ModelMode, ModelsResult, Outcome, SETUP_EXE, cli, delete_key_tree, read_install_marker,
    read_setup_manifest, read_trailer, verify,
};

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

struct PackTestEnv {
    root: PathBuf,
    reg_prefix: String,
    ctx: Ctx,
}

impl PackTestEnv {
    fn new(label: &str) -> Self {
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let unique = format!("pack-{label}-{pid}-{id}");
        let base = std::env::var_os("BR_TEST_TMP")
            .filter(|v| !v.to_string_lossy().trim().is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(r"K:\scratch\br-rust\test-tmp"));
        let root = base.join(&unique);
        fs::create_dir_all(&root).expect("create pack test root");
        let reg_prefix = format!(r"Software\BetterRecallTests\{unique}");
        let ctx = Ctx::for_test(&root, &reg_prefix);
        Self {
            root,
            reg_prefix,
            ctx,
        }
    }
}

impl Drop for PackTestEnv {
    fn drop(&mut self) {
        let _ = delete_key_tree(&self.reg_prefix);
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn find_debug_installer_stub() -> PathBuf {
    let br_sign = Path::new(env!("CARGO_BIN_EXE_br-sign"));
    let sibling = br_sign.parent().unwrap().join("br-installer.exe");
    if sibling.is_file() {
        sibling
    } else {
        // Fallback when running `cargo test -p br-sign` without building the `br-installer` bin.
        br_sign.to_path_buf()
    }
}

#[test]
fn pack_round_trips_and_installs_silently_on_dev_root() {
    let env = PackTestEnv::new("demo");
    let layout = env.root.join("layout");
    fs::create_dir_all(layout.join("worker")).expect("create worker dir");
    fs::write(layout.join(InstallLayout::AGENT_EXE), b"demo-agent-bytes").expect("write agent");
    fs::write(layout.join(InstallLayout::CLI_EXE), b"demo-cli-bytes").expect("write cli");
    fs::write(layout.join(InstallLayout::MCP_EXE), b"demo-mcp-bytes").expect("write mcp");
    fs::write(layout.join(InstallLayout::WORKER_EXE), b"demo-worker-bytes").expect("write worker");
    fs::write(layout.join("LICENSE"), b"AGPL-3.0\n").expect("write license");
    // BetterRecall-Setup.exe in the layout (placed there for manifest signing) must be skipped by pack.
    fs::write(layout.join(SETUP_EXE), b"stub-copy-in-layout").expect("write setup copy");

    let stub = find_debug_installer_stub();
    let stub_size = fs::metadata(&stub).expect("stub metadata").len();
    let packed = env.root.join("BetterRecall-Setup-0.3.0-dev.1.exe");

    let output = Command::new(env!("CARGO_BIN_EXE_br-sign"))
        .args([
            "pack",
            "--stub",
            stub.to_str().unwrap(),
            "--layout",
            layout.to_str().unwrap(),
            "--version",
            "0.3.0-dev.1",
            "--out",
            packed.to_str().unwrap(),
        ])
        .output()
        .expect("run br-sign pack");
    assert!(
        output.status.success(),
        "br-sign pack failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let trailer = read_trailer(&packed).expect("read_trailer");
    assert_eq!(trailer.offset, stub_size);
    verify(&packed, &trailer).expect("verify payload");

    let manifest = read_setup_manifest(&packed, &trailer).expect("read_setup_manifest");
    assert_eq!(manifest.version, "0.3.0-dev.1");
    assert_eq!(manifest.files, 5);

    // Silent install on dev root via dispatch_silent_for_exe.
    let args = cli::parse_args([
        "/S",
        "/norun",
        "/models=later",
        "--dev-root",
        env.root.to_str().unwrap(),
        "--dev-reg-prefix",
        &env.reg_prefix,
    ])
    .expect("parse args");
    let outcome =
        cli::dispatch_silent_for_exe(&env.ctx, &args, &packed).expect("silent install of packed");
    assert_eq!(
        outcome,
        Outcome::Installed {
            version: "0.3.0-dev.1".to_string(),
            models: ModelsResult::Skipped,
        }
    );

    let marker = read_install_marker(&env.ctx.install_dir).expect(".install marker");
    assert_eq!(marker.version, "0.3.0-dev.1");
    assert_eq!(marker.file_count, 5);
    assert_eq!(
        fs::read(env.ctx.install_dir.join(InstallLayout::AGENT_EXE)).unwrap(),
        b"demo-agent-bytes"
    );
    assert_eq!(
        fs::read(env.ctx.install_dir.join(InstallLayout::WORKER_EXE)).unwrap(),
        b"demo-worker-bytes"
    );
    assert_eq!(
        fs::metadata(env.ctx.install_dir.join(SETUP_EXE))
            .expect("installed setup exe")
            .len(),
        stub_size
    );
    assert_eq!(
        br_installer::read_model_mode(&env.ctx),
        Some(ModelMode::Later)
    );
    assert_eq!(
        fs::read(br_installer::relay_path(&env.ctx.home)).unwrap(),
        b"demo-mcp-bytes"
    );
    assert!(br_installer::cli_shim_path(&env.ctx.home).is_file());
}
