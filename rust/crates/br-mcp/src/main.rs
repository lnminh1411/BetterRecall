// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! `betterrecall-mcp.exe`: MCP server (inside install folder) or MCP relay (outside install folder).

use std::io::{self, BufReader};
use std::path::{Path, PathBuf};

use br_contracts::{DataPaths, InstallLayout, PipeNames};
use br_core::win32;
use br_integrity::{ProgramKind, enforce_or_exit};
use br_mcp::{
    HostOptions, McpHost, McpPipe, McpServer, RelayConfig, RelayDeps, is_server_role,
    read_registry_install_path, resolve_install_dir, run_relay, start_real_server,
};

fn main() {
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."));

    let code = if is_server_role(&exe_dir) {
        run_server(&exe_dir)
    } else {
        run_relay_role()
    };
    std::process::exit(code);
}

fn run_server(exe_dir: &Path) -> i32 {
    enforce_or_exit(exe_dir, ProgramKind::Headless);

    let home = DataPaths::home();
    let sid = PipeNames::user_sid().unwrap_or_default();
    let pipe_name = PipeNames::mcp(&sid, &home);
    let agent_exe = InstallLayout::full(exe_dir, InstallLayout::AGENT_EXE);
    let setup_mutex = PipeNames::setup_mutex_for(&home);
    let relayed = std::env::var(InstallLayout::MCP_RELAY_ENV).is_ok_and(|v| v == "1");

    let pipe = McpPipe::new(
        pipe_name,
        agent_exe.to_string_lossy().into_owned(),
        setup_mutex,
        relayed,
    );
    let pipe_ref = &pipe;
    let mut server = McpServer::new(|line| pipe_ref.call(line));

    let stdin = BufReader::new(io::stdin());
    let mut stdout = io::stdout().lock();
    McpHost::run(stdin, &mut stdout, &mut server, &pipe, HostOptions::default())
}

fn run_relay_role() -> i32 {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (install_dir, forward_args) = match resolve_install_dir(
        &args,
        read_registry_install_path,
        InstallLayout::default_install_dir,
    ) {
        Ok(pair) => pair,
        Err(msg) => {
            eprintln!("betterrecall-mcp: {msg}");
            return 1;
        }
    };

    let home = DataPaths::home();
    let setup_mutex = PipeNames::setup_mutex_for(&home);
    let outcome = run_relay(RelayDeps {
        client_in: io::stdin(),
        client_out: io::stdout(),
        stderr: io::stderr(),
        install_dir,
        mutex_exists: move || win32::mutex_exists(&setup_mutex),
        start_child: move |dir: &Path| start_real_server(dir, &forward_args),
        config: RelayConfig::default(),
    });
    outcome.exit_code
}
