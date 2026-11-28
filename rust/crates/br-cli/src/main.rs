// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! `betterrecall.exe`: CLI client for BetterRecall's public IPC pipe.

use std::io::{self, BufRead, IsTerminal, Write};
use std::marker::PhantomData;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use br_cli::{CliEnv, PipeCliConnection, run_cli, run_mcp_cli};
use br_contracts::{DataPaths, InstallLayout, PipeNames};
use br_core::{AgentPresence, CancelSource, PipeClient, SettingsStore};
use br_integrity::{ProgramKind, enforce_or_exit};
use br_mcp_clients::ClientEnv;
use windows::Win32::System::Console::SetConsoleCtrlHandler;
use windows::core::BOOL;

static CTRL_C_SOURCE: OnceLock<CancelSource> = OnceLock::new();

unsafe extern "system" fn ctrl_handler(_ctrl_type: u32) -> BOOL {
    if let Some(src) = CTRL_C_SOURCE.get() {
        src.cancel();
    }
    BOOL(1)
}

fn install_ctrl_c_handler() -> br_core::CancelToken {
    let src = CTRL_C_SOURCE.get_or_init(CancelSource::new);
    let _ = unsafe { SetConsoleCtrlHandler(Some(ctrl_handler), true) };
    src.token()
}

fn main() {
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."));
    enforce_or_exit(&exe_dir, ProgramKind::Headless);

    let args: Vec<String> = std::env::args().skip(1).collect();
    let home = DataPaths::home();

    if args.first().is_some_and(|a| a.eq_ignore_ascii_case("mcp")) {
        let mut stdout = io::stdout().lock();
        let mut stderr = io::stderr().lock();
        let Some(client_env) = ClientEnv::from_system() else {
            let _ = writeln!(stderr, "could not determine USERPROFILE or APPDATA");
            std::process::exit(1);
        };
        let relay = DataPaths::mcp_relay(&home);
        let code = run_mcp_cli(&args[1..], &client_env, &relay, &mut stdout, &mut stderr);
        std::process::exit(code);
    }

    let settings = SettingsStore::read_only(&home);
    let sid = PipeNames::user_sid().unwrap_or_default();
    let pipe = PipeNames::public(&settings.ipc.pipe_name, &sid, &home);
    let agent_exe = InstallLayout::full(&exe_dir, InstallLayout::AGENT_EXE);
    let agent_exe_str = agent_exe.to_string_lossy().into_owned();

    let ct = install_ctrl_c_handler();
    let interactive = io::stdin().is_terminal();

    let mut stdout = io::stdout().lock();
    let mut stderr = io::stderr().lock();

    let code = run_cli(
        &args,
        CliEnv {
            connect: || {
                let (res, client) = PipeClient::connect(&pipe, &agent_exe_str, 1000, &ct);
                (res, client.map(|c| PipeCliConnection::new(c, &ct)))
            },
            not_answering: || AgentPresence::not_answering(&home, settings.ipc.enabled),
            interactive,
            read_answer: || {
                let mut line = String::new();
                let mut stdin = io::stdin().lock();
                match stdin.read_line(&mut line) {
                    Ok(0) | Err(_) => None,
                    Ok(_) => Some(line),
                }
            },
            stdout: &mut stdout,
            stderr: &mut stderr,
            _marker: PhantomData,
        },
    );

    std::process::exit(code);
}
