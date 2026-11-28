// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Discover and configure MCP clients for BetterRecall.
//!
//! Pure library with no terminal I/O so both `betterrecall mcp ...` and a future GPUI settings
//! page can share the same implementation. All environment paths are injected via [`ClientEnv`].

mod clients;
mod io;
mod jsonc;
mod toml_cfg;

use std::env;
use std::path::{Path, PathBuf};

/// The MCP server entry name registered in every client configuration.
pub const SERVER_NAME: &str = "betterrecall";

/// Injected environment paths used to locate MCP client configuration files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientEnv {
    /// User profile directory (`%USERPROFILE%`).
    pub home: PathBuf,
    /// Roaming AppData directory (`%APPDATA%`).
    pub appdata: PathBuf,
    /// Optional XDG config home (`XDG_CONFIG_HOME`).
    pub xdg_config_home: Option<PathBuf>,
    /// Optional Codex home (`CODEX_HOME`).
    pub codex_home: Option<PathBuf>,
}

impl ClientEnv {
    /// Reads `USERPROFILE`, `APPDATA`, `XDG_CONFIG_HOME`, and `CODEX_HOME` from the process
    /// environment. Returns `None` if `USERPROFILE` or `APPDATA` is unset or empty.
    pub fn from_system() -> Option<Self> {
        let home = non_empty_env("USERPROFILE")?;
        let appdata = non_empty_env("APPDATA")?;
        let xdg_config_home = non_empty_env("XDG_CONFIG_HOME");
        let codex_home = non_empty_env("CODEX_HOME");
        Some(Self {
            home,
            appdata,
            xdg_config_home,
            codex_home,
        })
    }
}

fn non_empty_env(name: &str) -> Option<PathBuf> {
    let val = env::var_os(name)?;
    if val.to_string_lossy().trim().is_empty() {
        return None;
    }
    Some(PathBuf::from(val))
}

/// Configuration file format for writable MCP clients.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigFormat {
    Json,
    Jsonc,
    Toml,
}

/// Whether a client is configured by editing a file or by copying a snippet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientKind {
    File { path: PathBuf, format: ConfigFormat },
    Snippet,
}

/// Observed state of the `betterrecall` entry in a client's configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntryState {
    Absent,
    Present {
        command: Option<String>,
        matches: bool,
    },
    /// Snippet-only clients whose config is not read or written directly.
    Unknown,
    /// Existing config file could not be read or parsed.
    Unreadable(String),
}

/// Status snapshot for a single known MCP client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientStatus {
    pub id: &'static str,
    pub name: &'static str,
    pub kind: ClientKind,
    pub detected: bool,
    pub state: EntryState,
}

/// Outcome of an [`add`] or [`remove`] operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeKind {
    Added,
    Updated,
    Unchanged,
    Removed,
    NotPresent,
}

/// Result of modifying (or checking) a writable client's config file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    pub kind: ChangeKind,
    pub path: PathBuf,
    pub backup: Option<PathBuf>,
}

/// Copy-pasteable configuration snippet (and optional CLI command) for a client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snippet {
    pub text: String,
    pub command: Option<String>,
    pub target_hint: String,
}

/// Errors returned by [`add`], [`remove`], and [`snippet`].
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("unknown MCP client '{0}'")]
    UnknownClient(String),
    #[error("client '{0}' does not support direct config edits; use snippet instead")]
    SnippetOnly(String),
    #[error("malformed config file {}: {detail}", path.display())]
    Malformed { path: PathBuf, detail: String },
    #[error("I/O error on {}: {source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("client '{0}' is not detected on this machine")]
    NotDetected(String),
}

/// Lists all known MCP clients along with their detection status and `betterrecall` entry state.
pub fn list(env: &ClientEnv, relay: &Path) -> Vec<ClientStatus> {
    let relay_str = relay.to_string_lossy();
    clients::ALL_CLIENTS
        .iter()
        .map(|spec| clients::inspect_client(spec, env, &relay_str))
        .collect()
}

/// Adds or updates the `betterrecall` MCP server entry for the given writable client.
pub fn add(env: &ClientEnv, id: &str, relay: &Path) -> Result<Change, ClientError> {
    let spec = clients::find_client(id).ok_or_else(|| ClientError::UnknownClient(id.to_string()))?;
    let relay_str = relay.to_string_lossy();
    clients::add_client(spec, env, &relay_str)
}

/// Removes the `betterrecall` MCP server entry from the given writable client.
pub fn remove(env: &ClientEnv, id: &str) -> Result<Change, ClientError> {
    let spec = clients::find_client(id).ok_or_else(|| ClientError::UnknownClient(id.to_string()))?;
    clients::remove_client(spec, env)
}

/// Returns a copy-pasteable configuration snippet (and optional CLI command) for the given client.
pub fn snippet(env: &ClientEnv, id: &str, relay: &Path) -> Result<Snippet, ClientError> {
    let spec = clients::find_client(id).ok_or_else(|| ClientError::UnknownClient(id.to_string()))?;
    let relay_str = relay.to_string_lossy();
    Ok(clients::build_snippet(spec, env, &relay_str))
}
