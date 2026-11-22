// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! MCP pipe client (`McpPipe`) and stdio loop (`McpHost`).
//!
//! Under the fixed per-user install folder (R6), staleness is detected via the installer's scoped
//! setup mutex (`PipeNames::setup_mutex_for(home)`): when it exists, an install, upgrade or
//! uninstall is running. Under the relay (`BETTERRECALL_MCP_RELAY=1`), a stale server exits with
//! `InstallLayout::MCP_RESTART_EXIT_CODE` (75) without answering (whether idle or when a request
//! arrives) so the installer finds the install folder free.

use std::io::{BufRead, Write};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use br_contracts::{Envelope, InstallLayout};
use br_core::{CancelToken, PipeClient, PipeConnectResult, win32};
use serde_json::Value;

use crate::server::{McpServer, PipeAnswer};

/// Staleness query interface used by [`McpHost::run`].
pub trait StaleState {
    fn is_stale_now(&self) -> bool;
    fn marked_stale(&self) -> bool;
    fn relayed(&self) -> bool;
}

/// One tool call = one request on the agent's MCP pipe after the client trust check.
pub struct McpPipe {
    pipe_name: String,
    agent_exe: String,
    setup_mutex: String,
    relayed: bool,
    stale_grace: Duration,
    stale: AtomicBool,
}

impl McpPipe {
    pub const NOT_TRUSTED: &'static str = "BetterRecall not trusted.";
    pub const UPDATED: &'static str = "BetterRecall was updated while this MCP session ran: restart your MCP client (or its BetterRecall server) to reconnect.";
    pub const NOT_RUNNING: &'static str =
        "BetterRecall not running, or MCP is off (setting mcp.enabled).";
    pub const NO_RESPONSE: &'static str = "No response from BetterRecall.";

    pub fn new(
        pipe_name: impl Into<String>,
        agent_exe: impl Into<String>,
        setup_mutex: impl Into<String>,
        relayed: bool,
    ) -> Self {
        Self {
            pipe_name: pipe_name.into(),
            agent_exe: agent_exe.into(),
            setup_mutex: setup_mutex.into(),
            relayed,
            stale_grace: Duration::from_secs(2),
            stale: AtomicBool::new(false),
        }
    }

    pub fn with_stale_grace(mut self, grace: Duration) -> Self {
        self.stale_grace = grace;
        self
    }

    /// True when the scoped setup mutex currently exists.
    pub fn is_stale(&self) -> bool {
        win32::mutex_exists(&self.setup_mutex)
    }

    pub fn call(&self, line: &str) -> PipeAnswer {
        if self.is_stale() {
            self.stale.store(true, Ordering::Release);
            return PipeAnswer::err(Self::UPDATED);
        }

        let ct = CancelToken::none();
        let (result, client) = PipeClient::connect(&self.pipe_name, &self.agent_exe, 1000, &ct);
        if result == PipeConnectResult::NotRunning {
            if self.is_stale() {
                self.stale.store(true, Ordering::Release);
                return PipeAnswer::err(Self::UPDATED);
            }
            return PipeAnswer::err(Self::NOT_RUNNING);
        }
        let Some(mut client) = client else {
            if self.is_stale() {
                self.stale.store(true, Ordering::Release);
                return PipeAnswer::err(Self::UPDATED);
            }
            return PipeAnswer::err(Self::NOT_TRUSTED);
        };
        if result != PipeConnectResult::Ok {
            if self.is_stale() {
                self.stale.store(true, Ordering::Release);
                return PipeAnswer::err(Self::UPDATED);
            }
            return PipeAnswer::err(Self::NOT_TRUSTED);
        }

        let response = client.request(line, &ct).ok().flatten();
        let Some(response) = response else {
            if self.wait_for_stale() {
                self.stale.store(true, Ordering::Release);
                return PipeAnswer::err(Self::UPDATED);
            }
            return PipeAnswer::err(Self::NO_RESPONSE);
        };

        // Raw data text, as the C# server's `GetRawText()`: the agent's key order is kept.
        match Envelope::parse_raw(&response) {
            Ok((true, data, _)) => PipeAnswer::ok(data.unwrap_or_else(|| "null".to_string())),
            Ok((false, _, error)) => {
                PipeAnswer::err(error.unwrap_or_else(|| "Internal error.".to_string()))
            }
            Err(_) => PipeAnswer::err("Internal error."),
        }
    }

    fn wait_for_stale(&self) -> bool {
        if self.is_stale() {
            return true;
        }
        let deadline = Instant::now() + self.stale_grace;
        while Instant::now() < deadline {
            thread::sleep(Duration::from_millis(50));
            if self.is_stale() {
                return true;
            }
        }
        self.is_stale()
    }
}

impl StaleState for McpPipe {
    fn is_stale_now(&self) -> bool {
        self.is_stale()
    }

    fn marked_stale(&self) -> bool {
        self.stale.load(Ordering::Acquire)
    }

    fn relayed(&self) -> bool {
        self.relayed
    }
}

/// Tuning options for [`McpHost::run`].
#[derive(Debug, Clone, Copy)]
pub struct HostOptions {
    pub poll_interval: Duration,
}

impl Default for HostOptions {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_millis(250),
        }
    }
}

/// The stdio loop: one JSON-RPC message per line in, one reply line out (none for notifications).
pub struct McpHost;

impl McpHost {
    /// Returns the process exit code: 0 at end of input, `MCP_RESTART_EXIT_CODE` (75) once stale.
    pub fn run<R, W, F>(
        mut input: R,
        output: &mut W,
        server: &mut McpServer<F>,
        stale: &dyn StaleState,
        opts: HostOptions,
    ) -> i32
    where
        R: BufRead + Send + 'static,
        W: Write,
        F: FnMut(&str) -> PipeAnswer,
    {
        let (tx, rx) = mpsc::channel::<String>();
        thread::spawn(move || {
            let mut buf = String::new();
            loop {
                buf.clear();
                match input.read_line(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        let trimmed = buf.trim_end_matches(['\r', '\n']).to_string();
                        if tx.send(trimmed).is_err() {
                            break;
                        }
                    }
                }
            }
        });

        loop {
            match rx.recv_timeout(opts.poll_interval) {
                Err(RecvTimeoutError::Timeout) => {
                    if stale.relayed() && stale.is_stale_now() {
                        return InstallLayout::MCP_RESTART_EXIT_CODE;
                    }
                }
                Err(RecvTimeoutError::Disconnected) => return 0,
                Ok(raw_line) => {
                    if raw_line.is_empty() {
                        continue;
                    }
                    let line = raw_line.strip_prefix('\u{FEFF}').unwrap_or(&raw_line);
                    if stale.relayed() && stale.is_stale_now() {
                        return InstallLayout::MCP_RESTART_EXIT_CODE;
                    }

                    let reply = match catch_unwind(AssertUnwindSafe(|| server.handle(line))) {
                        Ok(r) => r,
                        Err(_) => {
                            eprintln!("betterrecall-mcp: internal panic");
                            Self::internal_error(line)
                        }
                    };

                    if stale.marked_stale() && stale.relayed() {
                        return InstallLayout::MCP_RESTART_EXIT_CODE;
                    }
                    if let Some(reply) = reply
                        && (writeln!(output, "{reply}").is_err() || output.flush().is_err())
                    {
                        return 0;
                    }
                    if stale.marked_stale() || (!stale.relayed() && stale.is_stale_now()) {
                        let _ = output.flush();
                        return InstallLayout::MCP_RESTART_EXIT_CODE;
                    }
                }
            }
        }
    }

    /// JSON-RPC `"Internal error"` for the request in `line` (string or number id), or `None`.
    pub fn internal_error(line: &str) -> Option<String> {
        let root: Value = serde_json::from_str(line).ok()?;
        let obj = root.as_object()?;
        let id = obj.get("id")?;
        if !matches!(id, Value::String(_) | Value::Number(_)) {
            return None;
        }
        let id_raw = serde_json::to_string(id).ok()?;
        Some(format!(
            r#"{{"jsonrpc":"2.0","id":{id_raw},"error":{{"code":-32603,"message":"Internal error"}}}}"#
        ))
    }
}
