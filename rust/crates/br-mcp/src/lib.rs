// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! BetterRecall MCP server and MCP relay (`betterrecall-mcp.exe`).
//!
//! Role is chosen at startup by checking whether `BetterRecall.Agent.exe` sits in the exe's own
//! folder:
//! - Present -> server role (JSON-RPC 2.0 over stdio talking to the agent's MCP pipe).
//! - Absent -> relay role (spawns and restarts the installed `betterrecall-mcp.exe` across
//!   installer/updater runs while keeping the MCP client's stdio session alive).

pub mod host;
pub mod relay;
pub mod server;

use std::path::Path;

use br_contracts::InstallLayout;

pub use host::{HostOptions, McpHost, McpPipe, StaleState};
pub use relay::{
    ChildHandles, ChildWaiter, LineSplitter, RelayConfig, RelayDeps, RelayOutcome, StartError,
    message_id, read_registry_install_path, resolve_install_dir, run_relay, start_real_server,
};
pub use server::{McpServer, PipeAnswer};

/// Returns `true` when `exe_dir` is an install folder (contains `BetterRecall.Agent.exe`).
pub fn is_server_role(exe_dir: &Path) -> bool {
    exe_dir.join(InstallLayout::AGENT_EXE).is_file()
}
