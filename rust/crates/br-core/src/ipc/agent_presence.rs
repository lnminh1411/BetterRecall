// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Agent presence detection via the per-home single-instance mutex (`AgentPresence.cs`).

use std::path::Path;

use br_contracts::PipeNames;

use crate::win32;

/// Why the public pipe did not answer (sweep 3 D8): the agent of this data folder holds its
/// single-instance mutex while it runs, so a held mutex with no pipe means scripts (IPC) are off,
/// not that BetterRecall is down.
pub struct AgentPresence;

impl AgentPresence {
    pub const ERR_NOT_RUNNING: &'static str = "err BetterRecall not running";
    pub const ERR_IPC_OFF: &'static str =
        "err Scripts (IPC) are off. Turn on Settings > IPC and MCP > Allow scripts (IPC).";

    /// True when an agent holds the mutex of this data folder.
    pub fn running(home: &Path) -> bool {
        win32::mutex_exists(&PipeNames::agent_mutex_for(home))
    }

    /// The CLI line for a pipe that did not answer, and its exit code: 3 when the agent runs and
    /// settings have `ipc.enabled` off, else 2 (not running, or running with IPC on but not
    /// answering in time).
    pub fn not_answering(home: &Path, ipc_enabled: bool) -> (&'static str, i32) {
        if !ipc_enabled && Self::running(home) {
            (Self::ERR_IPC_OFF, 3)
        } else {
            (Self::ERR_NOT_RUNNING, 2)
        }
    }
}
