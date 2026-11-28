// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Static registry of supported MCP clients, path resolution, detection, and snippet generation.

use std::path::{Path, PathBuf};

use crate::io::{FileStyle, read_text_file, write_text_file};
use crate::jsonc::{add_json, inspect_json, json_string_literal, remove_json, render_new_json};
use crate::toml_cfg::{add_toml, inspect_toml, remove_toml, render_new_toml};
use crate::{
    Change, ChangeKind, ClientEnv, ClientError, ClientKind, ClientStatus, ConfigFormat, EntryState,
    SERVER_NAME, Snippet,
};

#[derive(Clone, Copy)]
pub(crate) enum ClientMode {
    Writable(WritableSpec),
    Snippet(SnippetKind),
}

#[derive(Clone, Copy)]
pub(crate) struct WritableSpec {
    pub format: ConfigFormat,
    pub servers_key: &'static str,
    pub extra_kv: Option<(&'static str, &'static str)>,
    pub needs_schema_version: bool,
    pub path_fn: fn(&ClientEnv) -> PathBuf,
    pub detect_dirs_fn: fn(&ClientEnv) -> Vec<PathBuf>,
}

#[derive(Clone, Copy)]
pub(crate) enum SnippetKind {
    ClaudeCode,
    DeepSeekHarness,
    Zed,
    Cline,
    Other,
}

#[derive(Clone, Copy)]
pub(crate) struct ClientSpec {
    pub id: &'static str,
    pub name: &'static str,
    pub mode: ClientMode,
}

fn claude_desktop_dir(env: &ClientEnv) -> PathBuf {
    env.appdata.join("Claude")
}
fn claude_desktop_file(env: &ClientEnv) -> PathBuf {
    claude_desktop_dir(env).join("claude_desktop_config.json")
}
fn claude_desktop_detect(env: &ClientEnv) -> Vec<PathBuf> {
    vec![claude_desktop_dir(env)]
}

fn antigravity_file(env: &ClientEnv) -> PathBuf {
    env.home
        .join(".gemini")
        .join("config")
        .join("mcp_config.json")
}
fn antigravity_detect(env: &ClientEnv) -> Vec<PathBuf> {
    let gemini = env.home.join(".gemini");
    vec![
        gemini.join("antigravity"),
        gemini.join("antigravity-cli"),
        gemini.join("antigravity-ide"),
    ]
}

fn gemini_cli_dir(env: &ClientEnv) -> PathBuf {
    env.home.join(".gemini")
}
fn gemini_cli_file(env: &ClientEnv) -> PathBuf {
    gemini_cli_dir(env).join("settings.json")
}
fn gemini_cli_detect(env: &ClientEnv) -> Vec<PathBuf> {
    vec![gemini_cli_dir(env)]
}

fn cursor_dir(env: &ClientEnv) -> PathBuf {
    env.home.join(".cursor")
}
fn cursor_file(env: &ClientEnv) -> PathBuf {
    cursor_dir(env).join("mcp.json")
}
fn cursor_detect(env: &ClientEnv) -> Vec<PathBuf> {
    vec![cursor_dir(env)]
}

fn windsurf_dir(env: &ClientEnv) -> PathBuf {
    env.home.join(".codeium").join("windsurf")
}
fn windsurf_file(env: &ClientEnv) -> PathBuf {
    windsurf_dir(env).join("mcp_config.json")
}
fn windsurf_detect(env: &ClientEnv) -> Vec<PathBuf> {
    vec![windsurf_dir(env)]
}

fn vscode_dir(env: &ClientEnv) -> PathBuf {
    env.appdata.join("Code").join("User")
}
fn vscode_file(env: &ClientEnv) -> PathBuf {
    vscode_dir(env).join("mcp.json")
}
fn vscode_detect(env: &ClientEnv) -> Vec<PathBuf> {
    vec![vscode_dir(env)]
}

fn muse_dir(env: &ClientEnv) -> PathBuf {
    env.xdg_config_home
        .clone()
        .unwrap_or_else(|| env.home.join(".config"))
        .join("muse")
}
fn muse_file(env: &ClientEnv) -> PathBuf {
    muse_dir(env).join("settings.json")
}
fn muse_detect(env: &ClientEnv) -> Vec<PathBuf> {
    vec![muse_dir(env)]
}

fn codex_dir(env: &ClientEnv) -> PathBuf {
    env.codex_home
        .clone()
        .unwrap_or_else(|| env.home.join(".codex"))
}
fn codex_file(env: &ClientEnv) -> PathBuf {
    codex_dir(env).join("config.toml")
}
fn codex_detect(env: &ClientEnv) -> Vec<PathBuf> {
    vec![codex_dir(env)]
}

pub(crate) const ALL_CLIENTS: &[ClientSpec] = &[
    ClientSpec {
        id: "claude-desktop",
        name: "Claude Desktop",
        mode: ClientMode::Writable(WritableSpec {
            format: ConfigFormat::Json,
            servers_key: "mcpServers",
            extra_kv: None,
            needs_schema_version: false,
            path_fn: claude_desktop_file,
            detect_dirs_fn: claude_desktop_detect,
        }),
    },
    ClientSpec {
        id: "antigravity",
        name: "Antigravity (CLI and desktop)",
        mode: ClientMode::Writable(WritableSpec {
            format: ConfigFormat::Json,
            servers_key: "mcpServers",
            extra_kv: None,
            needs_schema_version: false,
            path_fn: antigravity_file,
            detect_dirs_fn: antigravity_detect,
        }),
    },
    ClientSpec {
        id: "gemini-cli",
        name: "Gemini CLI",
        mode: ClientMode::Writable(WritableSpec {
            format: ConfigFormat::Jsonc,
            servers_key: "mcpServers",
            extra_kv: None,
            needs_schema_version: false,
            path_fn: gemini_cli_file,
            detect_dirs_fn: gemini_cli_detect,
        }),
    },
    ClientSpec {
        id: "cursor",
        name: "Cursor",
        mode: ClientMode::Writable(WritableSpec {
            format: ConfigFormat::Json,
            servers_key: "mcpServers",
            extra_kv: None,
            needs_schema_version: false,
            path_fn: cursor_file,
            detect_dirs_fn: cursor_detect,
        }),
    },
    ClientSpec {
        id: "windsurf",
        name: "Windsurf",
        mode: ClientMode::Writable(WritableSpec {
            format: ConfigFormat::Json,
            servers_key: "mcpServers",
            extra_kv: None,
            needs_schema_version: false,
            path_fn: windsurf_file,
            detect_dirs_fn: windsurf_detect,
        }),
    },
    ClientSpec {
        id: "vscode",
        name: "VS Code (Copilot)",
        mode: ClientMode::Writable(WritableSpec {
            format: ConfigFormat::Jsonc,
            servers_key: "servers",
            extra_kv: Some(("type", "stdio")),
            needs_schema_version: false,
            path_fn: vscode_file,
            detect_dirs_fn: vscode_detect,
        }),
    },
    ClientSpec {
        id: "muse-code",
        name: "Muse Code (Meta)",
        mode: ClientMode::Writable(WritableSpec {
            format: ConfigFormat::Json,
            servers_key: "mcp_servers",
            extra_kv: Some(("transport", "stdio")),
            needs_schema_version: true,
            path_fn: muse_file,
            detect_dirs_fn: muse_detect,
        }),
    },
    ClientSpec {
        id: "codex",
        name: "Codex CLI",
        mode: ClientMode::Writable(WritableSpec {
            format: ConfigFormat::Toml,
            servers_key: "mcp_servers",
            extra_kv: None,
            needs_schema_version: false,
            path_fn: codex_file,
            detect_dirs_fn: codex_detect,
        }),
    },
    ClientSpec {
        id: "claude-code",
        name: "Claude Code",
        mode: ClientMode::Snippet(SnippetKind::ClaudeCode),
    },
    ClientSpec {
        id: "deepseek-harness",
        name: "DeepSeek Harness",
        mode: ClientMode::Snippet(SnippetKind::DeepSeekHarness),
    },
    ClientSpec {
        id: "zed",
        name: "Zed",
        mode: ClientMode::Snippet(SnippetKind::Zed),
    },
    ClientSpec {
        id: "cline",
        name: "Cline",
        mode: ClientMode::Snippet(SnippetKind::Cline),
    },
    ClientSpec {
        id: "other",
        name: "Other clients",
        mode: ClientMode::Snippet(SnippetKind::Other),
    },
];

pub(crate) fn find_client(id: &str) -> Option<&'static ClientSpec> {
    ALL_CLIENTS.iter().find(|c| c.id == id)
}

fn is_writable_detected(spec: &WritableSpec, env: &ClientEnv, path: &Path) -> bool {
    path.exists() || (spec.detect_dirs_fn)(env).iter().any(|d| d.exists())
}

fn is_snippet_detected(kind: SnippetKind, env: &ClientEnv) -> bool {
    match kind {
        SnippetKind::ClaudeCode => env.home.join(".claude.json").exists(),
        SnippetKind::DeepSeekHarness => env.home.join(".dsh").exists(),
        SnippetKind::Zed | SnippetKind::Cline | SnippetKind::Other => false,
    }
}

pub(crate) fn inspect_client(
    spec: &ClientSpec,
    env: &ClientEnv,
    relay_str: &str,
    server_env: Option<(&str, &str)>,
) -> ClientStatus {
    match spec.mode {
        ClientMode::Writable(w) => {
            let path = (w.path_fn)(env);
            let detected = is_writable_detected(&w, env, &path);
            let state = match read_text_file(&path) {
                Ok(None) => EntryState::Absent,
                Ok(Some(file)) => {
                    let parsed = match w.format {
                        ConfigFormat::Json | ConfigFormat::Jsonc => inspect_json(
                            &file.body,
                            w.format,
                            w.servers_key,
                            w.extra_kv,
                            relay_str,
                            server_env,
                        ),
                        ConfigFormat::Toml => inspect_toml(&file.body, relay_str, server_env),
                    };
                    match parsed {
                        Ok(s) => s,
                        Err(detail) => EntryState::Unreadable(detail),
                    }
                }
                Err(ClientError::Malformed { detail, .. }) => EntryState::Unreadable(detail),
                Err(ClientError::Io { source, .. }) => EntryState::Unreadable(source.to_string()),
                Err(other) => EntryState::Unreadable(other.to_string()),
            };
            ClientStatus {
                id: spec.id,
                name: spec.name,
                kind: ClientKind::File {
                    path,
                    format: w.format,
                },
                detected,
                state,
            }
        }
        ClientMode::Snippet(s) => ClientStatus {
            id: spec.id,
            name: spec.name,
            kind: ClientKind::Snippet,
            detected: is_snippet_detected(s, env),
            state: EntryState::Unknown,
        },
    }
}

pub(crate) fn add_client(
    spec: &ClientSpec,
    env: &ClientEnv,
    relay_str: &str,
    server_env: Option<(&str, &str)>,
) -> Result<Change, ClientError> {
    let ClientMode::Writable(w) = spec.mode else {
        return Err(ClientError::SnippetOnly(spec.id.to_string()));
    };

    let path = (w.path_fn)(env);
    if !is_writable_detected(&w, env, &path) {
        return Err(ClientError::NotDetected(spec.id.to_string()));
    }

    let existing = read_text_file(&path)?;
    let (existed, style, body_opt) = match &existing {
        Some(tf) => (true, tf.style, Some(tf.body.as_str())),
        None => (false, FileStyle::default(), None),
    };

    let edited = match w.format {
        ConfigFormat::Json | ConfigFormat::Jsonc => add_json(
            body_opt,
            w.format,
            w.servers_key,
            w.extra_kv,
            w.needs_schema_version,
            relay_str,
            server_env,
        ),
        ConfigFormat::Toml => add_toml(body_opt, relay_str, server_env),
    }
    .map_err(|detail| ClientError::Malformed {
        path: path.clone(),
        detail,
    })?;

    let (kind, maybe_new_text) = edited;
    let Some(new_text) = maybe_new_text else {
        return Ok(Change {
            kind,
            path,
            backup: None,
        });
    };

    let backup = write_text_file(&path, &new_text, style, existed)?;
    Ok(Change { kind, path, backup })
}

pub(crate) fn remove_client(spec: &ClientSpec, env: &ClientEnv) -> Result<Change, ClientError> {
    let ClientMode::Writable(w) = spec.mode else {
        return Err(ClientError::SnippetOnly(spec.id.to_string()));
    };

    let path = (w.path_fn)(env);
    let Some(existing) = read_text_file(&path)? else {
        return Ok(Change {
            kind: ChangeKind::NotPresent,
            path,
            backup: None,
        });
    };

    let edited = match w.format {
        ConfigFormat::Json | ConfigFormat::Jsonc => {
            remove_json(&existing.body, w.format, w.servers_key)
        }
        ConfigFormat::Toml => remove_toml(&existing.body),
    }
    .map_err(|detail| ClientError::Malformed {
        path: path.clone(),
        detail,
    })?;

    let (kind, maybe_new_text) = edited;
    let Some(new_text) = maybe_new_text else {
        return Ok(Change {
            kind,
            path,
            backup: None,
        });
    };

    let backup = write_text_file(&path, &new_text, existing.style, true)?;
    Ok(Change { kind, path, backup })
}

fn yaml_single_quoted(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for ch in s.chars() {
        if ch == '\'' {
            out.push_str("''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

pub(crate) fn build_snippet(
    spec: &ClientSpec,
    env: &ClientEnv,
    relay_str: &str,
    server_env: Option<(&str, &str)>,
) -> Snippet {
    match spec.mode {
        ClientMode::Writable(w) => {
            let path = (w.path_fn)(env);
            let text = match w.format {
                ConfigFormat::Json | ConfigFormat::Jsonc => render_new_json(
                    w.servers_key,
                    w.extra_kv,
                    w.needs_schema_version,
                    relay_str,
                    server_env,
                ),
                ConfigFormat::Toml => render_new_toml(relay_str, server_env),
            };
            Snippet {
                text,
                command: None,
                target_hint: path.display().to_string(),
            }
        }
        ClientMode::Snippet(SnippetKind::ClaudeCode) => {
            let cmd = match server_env {
                Some((ek, ev)) => format!(
                    "claude mcp add --scope user -e {ek}=\"{ev}\" {SERVER_NAME} -- \"{relay_str}\""
                ),
                None => format!("claude mcp add --scope user {SERVER_NAME} -- \"{relay_str}\""),
            };
            let text = format!(
                "Run this command in your terminal (Claude Code manages ~/.claude.json itself):\n\n{cmd}\n"
            );
            Snippet {
                text,
                command: Some(cmd),
                target_hint: env.home.join(".claude.json").display().to_string(),
            }
        }
        ClientMode::Snippet(SnippetKind::DeepSeekHarness) => {
            let target = env
                .home
                .join(".dsh")
                .join("profiles")
                .join("web")
                .join("cordis.patch.yml");
            let cmd_yaml = yaml_single_quoted(relay_str);
            let env_block = match server_env {
                Some((ek, ev)) => format!(
                    "        env:\n          {ek}: {}\n",
                    yaml_single_quoted(ev)
                ),
                None => String::new(),
            };
            let text = format!(
                "# Merge this block into {} (do not replace the file), then restart DeepSeek Harness.\n\
                 - insert:\n\
                 \x20   - name: '@deepseek-ai/dsh-mcp-client'\n\
                 \x20     config:\n\
                 \x20       serverName: {SERVER_NAME}\n\
                 \x20       transport: stdio\n\
                 \x20       command: {cmd_yaml}\n\
                 \x20       args: []\n\
                 {env_block}",
                target.display()
            );
            Snippet {
                text,
                command: None,
                target_hint: target.display().to_string(),
            }
        }
        ClientMode::Snippet(SnippetKind::Zed) => {
            let cmd_json = json_string_literal(relay_str);
            let text = match server_env {
                Some((ek, ev)) => format!(
                    "\"context_servers\": {{\n  \"{SERVER_NAME}\": {{\n    \"command\": {cmd_json},\n    \"args\": [],\n    \"env\": {{\n      {}: {}\n    }}\n  }}\n}}\n",
                    json_string_literal(ek),
                    json_string_literal(ev)
                ),
                None => format!(
                    "\"context_servers\": {{\n  \"{SERVER_NAME}\": {{\n    \"command\": {cmd_json},\n    \"args\": []\n  }}\n}}\n"
                ),
            };
            Snippet {
                text,
                command: None,
                target_hint: "Zed settings.json".to_string(),
            }
        }
        ClientMode::Snippet(SnippetKind::Cline) => Snippet {
            text: render_new_json("mcpServers", None, false, relay_str, server_env),
            command: None,
            target_hint: "Cline MCP settings (cline_mcp_settings.json)".to_string(),
        },
        ClientMode::Snippet(SnippetKind::Other) => Snippet {
            text: render_new_json("mcpServers", None, false, relay_str, server_env),
            command: None,
            target_hint: "MCP client JSON configuration".to_string(),
        },
    }
}
