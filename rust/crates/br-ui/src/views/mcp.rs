// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use br_mcp_clients::{
    Change, ChangeKind, ClientEnv, ClientError, ClientKind, ClientStatus, ConfigFormat, EntryState,
    Snippet,
};
use gpui::{
    AppContext as _, Context, InteractiveElement as _, IntoElement, KeyDownEvent,
    ParentElement as _, Render, StatefulInteractiveElement as _, Styled as _, Window, div, px,
};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::{ActiveTheme as _, Disableable as _, h_flex, v_flex};

use crate::platform;
use crate::views::tip::render_tip;

pub const MOCK_RELAY_PATH: &str = r"%LOCALAPPDATA%\BetterRecall\app\bin\betterrecall-mcp.exe";

/// Abstraction over `br_mcp_clients` so mock mode never touches real user config files.
pub trait McpBackend: Send + Sync {
    fn list(&self) -> Vec<ClientStatus>;
    fn add(&self, id: &str) -> Result<Change, ClientError>;
    fn remove(&self, id: &str) -> Result<Change, ClientError>;
    fn snippet(&self, id: &str) -> Result<Snippet, ClientError>;
}

/// Real backend backed by `br_mcp_clients` using an explicitly injected [`ClientEnv`].
/// Never calls `ClientEnv::from_system()`.
pub struct RealMcpBackend {
    env: ClientEnv,
    relay: PathBuf,
}

impl RealMcpBackend {
    pub fn from_mock_home(root: &Path, relay: PathBuf) -> Self {
        let home = root.join("home");
        let appdata = root.join("appdata");
        let _ = std::fs::create_dir_all(&home);
        let _ = std::fs::create_dir_all(&appdata);
        Self::new(
            ClientEnv {
                home,
                appdata,
                xdg_config_home: None,
                codex_home: None,
            },
            relay,
        )
    }

    pub fn new(env: ClientEnv, relay: PathBuf) -> Self {
        Self { env, relay }
    }
}

impl McpBackend for RealMcpBackend {
    fn list(&self) -> Vec<ClientStatus> {
        br_mcp_clients::list(&self.env, &self.relay, None)
    }

    fn add(&self, id: &str) -> Result<Change, ClientError> {
        br_mcp_clients::add(&self.env, id, &self.relay, None)
    }

    fn remove(&self, id: &str) -> Result<Change, ClientError> {
        br_mcp_clients::remove(&self.env, id)
    }

    fn snippet(&self, id: &str) -> Result<Snippet, ClientError> {
        br_mcp_clients::snippet(&self.env, id, &self.relay, None)
    }
}

/// In-memory fake backend with three canned clients for safe mock mode without `--mock-mcp-home`.
pub struct FakeMcpBackend {
    clients: Mutex<Vec<ClientStatus>>,
    relay: String,
}

impl FakeMcpBackend {
    pub fn new(relay: impl Into<String>) -> Self {
        let relay = relay.into();
        let clients = vec![
            ClientStatus {
                id: "claude-desktop",
                name: "Claude Desktop",
                kind: ClientKind::File {
                    path: PathBuf::from("claude_desktop_config.json"),
                    format: ConfigFormat::Json,
                },
                detected: true,
                state: EntryState::Absent,
            },
            ClientStatus {
                id: "cursor",
                name: "Cursor",
                kind: ClientKind::File {
                    path: PathBuf::from("mcp.json"),
                    format: ConfigFormat::Json,
                },
                detected: true,
                state: EntryState::Present {
                    command: Some(relay.clone()),
                    matches: true,
                },
            },
            ClientStatus {
                id: "claude-code",
                name: "Claude Code CLI",
                kind: ClientKind::Snippet,
                detected: true,
                state: EntryState::Unknown,
            },
        ];
        Self {
            clients: Mutex::new(clients),
            relay,
        }
    }
}

impl McpBackend for FakeMcpBackend {
    fn list(&self) -> Vec<ClientStatus> {
        self.clients.lock().expect("fake mcp lock").clone()
    }

    fn add(&self, id: &str) -> Result<Change, ClientError> {
        let mut guard = self.clients.lock().expect("fake mcp lock");
        let client = guard
            .iter_mut()
            .find(|c| c.id == id)
            .ok_or_else(|| ClientError::UnknownClient(id.to_string()))?;

        let path = match &client.kind {
            ClientKind::File { path, .. } => path.clone(),
            ClientKind::Snippet => return Err(ClientError::SnippetOnly(id.to_string())),
        };
        if !client.detected {
            return Err(ClientError::NotDetected(id.to_string()));
        }

        let kind = match &client.state {
            EntryState::Present { matches: true, .. } => ChangeKind::Unchanged,
            EntryState::Present { matches: false, .. } => {
                client.state = EntryState::Present {
                    command: Some(self.relay.clone()),
                    matches: true,
                };
                ChangeKind::Updated
            }
            EntryState::Absent => {
                client.state = EntryState::Present {
                    command: Some(self.relay.clone()),
                    matches: true,
                };
                ChangeKind::Added
            }
            EntryState::Unknown => return Err(ClientError::SnippetOnly(id.to_string())),
            EntryState::Unreadable(detail) => {
                return Err(ClientError::Malformed {
                    path,
                    detail: detail.clone(),
                });
            }
        };

        let backup = matches!(kind, ChangeKind::Added | ChangeKind::Updated)
            .then(|| path.with_extension("json.bak-betterrecall"));
        Ok(Change { kind, path, backup })
    }

    fn remove(&self, id: &str) -> Result<Change, ClientError> {
        let mut guard = self.clients.lock().expect("fake mcp lock");
        let client = guard
            .iter_mut()
            .find(|c| c.id == id)
            .ok_or_else(|| ClientError::UnknownClient(id.to_string()))?;

        let path = match &client.kind {
            ClientKind::File { path, .. } => path.clone(),
            ClientKind::Snippet => return Err(ClientError::SnippetOnly(id.to_string())),
        };

        let kind = match &client.state {
            EntryState::Present { .. } => {
                client.state = EntryState::Absent;
                ChangeKind::Removed
            }
            EntryState::Absent => ChangeKind::NotPresent,
            EntryState::Unknown => return Err(ClientError::SnippetOnly(id.to_string())),
            EntryState::Unreadable(detail) => {
                return Err(ClientError::Malformed {
                    path,
                    detail: detail.clone(),
                });
            }
        };

        let backup =
            (kind == ChangeKind::Removed).then(|| path.with_extension("json.bak-betterrecall"));
        Ok(Change { kind, path, backup })
    }

    fn snippet(&self, id: &str) -> Result<Snippet, ClientError> {
        let guard = self.clients.lock().expect("fake mcp lock");
        let client = guard
            .iter()
            .find(|c| c.id == id)
            .ok_or_else(|| ClientError::UnknownClient(id.to_string()))?;

        let text = format!(
            "{{\n  \"mcpServers\": {{\n    \"betterrecall\": {{\n      \"command\": \"{}\"\n    }}\n  }}\n}}",
            self.relay.replace('\\', "\\\\")
        );
        let command = (client.id == "claude-code")
            .then(|| format!("claude mcp add --scope user betterrecall -- \"{}\"", self.relay));
        Ok(Snippet {
            text,
            command,
            target_hint: client.name.to_string(),
        })
    }
}

pub fn build_mock_backend(mock_mcp_home: Option<&Path>) -> Arc<dyn McpBackend> {
    match mock_mcp_home {
        Some(dir) => Arc::new(RealMcpBackend::from_mock_home(
            dir,
            PathBuf::from(MOCK_RELAY_PATH),
        )),
        None => Arc::new(FakeMcpBackend::new(MOCK_RELAY_PATH)),
    }
}

fn format_change_message(change: &Change) -> String {
    let verb = match change.kind {
        ChangeKind::Added => "Added",
        ChangeKind::Updated => "Updated",
        ChangeKind::Unchanged => "Already configured",
        ChangeKind::Removed => "Removed",
        ChangeKind::NotPresent => "Not configured",
    };
    match &change.backup {
        Some(bak) => format!("{verb} (backup: {})", bak.display()),
        None => verb.to_string(),
    }
}

fn entry_state_label(client: &ClientStatus) -> String {
    if matches!(client.kind, ClientKind::Snippet) {
        return "Copy the config".to_string();
    }
    match &client.state {
        EntryState::Present { matches: true, .. } => "Configured".to_string(),
        EntryState::Present { matches: false, .. } => "Configured, different command".to_string(),
        EntryState::Absent => "Not configured".to_string(),
        EntryState::Unreadable(reason) => format!("Can't read config: {reason}"),
        EntryState::Unknown => "Copy the config".to_string(),
    }
}

pub struct McpView {
    backend: Arc<dyn McpBackend>,
    clients: Vec<ClientStatus>,
    row_messages: HashMap<&'static str, (bool, String)>,
    busy_id: Option<&'static str>,
    tip_visible: bool,
}

impl McpView {
    pub fn new(mock_mcp_home: Option<&Path>, show_tip: bool) -> Self {
        let backend = build_mock_backend(mock_mcp_home);
        let clients = backend.list();
        Self {
            backend,
            clients,
            row_messages: HashMap::new(),
            busy_id: None,
            tip_visible: show_tip,
        }
    }

    fn run_add(&mut self, id: &'static str, cx: &mut Context<Self>) {
        if self.busy_id.is_some() {
            return;
        }
        self.busy_id = Some(id);
        cx.notify();

        let backend = self.backend.clone();
        cx.spawn(async move |weak, cx| {
            let (res, refreshed) = cx
                .background_spawn(async move {
                    let res = backend.add(id);
                    let refreshed = backend.list();
                    (res, refreshed)
                })
                .await;

            let _ = weak.update(cx, |this, cx| {
                this.clients = refreshed;
                this.busy_id = None;
                match res {
                    Ok(change) => {
                        this.row_messages
                            .insert(id, (false, format_change_message(&change)));
                    }
                    Err(e) => {
                        this.row_messages.insert(id, (true, e.to_string()));
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn run_remove(&mut self, id: &'static str, cx: &mut Context<Self>) {
        if self.busy_id.is_some() {
            return;
        }
        self.busy_id = Some(id);
        cx.notify();

        let backend = self.backend.clone();
        cx.spawn(async move |weak, cx| {
            let (res, refreshed) = cx
                .background_spawn(async move {
                    let res = backend.remove(id);
                    let refreshed = backend.list();
                    (res, refreshed)
                })
                .await;

            let _ = weak.update(cx, |this, cx| {
                this.clients = refreshed;
                this.busy_id = None;
                match res {
                    Ok(change) => {
                        this.row_messages
                            .insert(id, (false, format_change_message(&change)));
                    }
                    Err(e) => {
                        this.row_messages.insert(id, (true, e.to_string()));
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn copy_config(&mut self, id: &'static str, cx: &mut Context<Self>) {
        match self.backend.snippet(id) {
            Ok(snip) => match platform::clipboard::copy_text_excluded(&snip.text) {
                Ok(()) => {
                    self.row_messages
                        .insert(id, (false, "Copied config to clipboard".to_string()));
                }
                Err(e) => {
                    self.row_messages
                        .insert(id, (true, format!("Clipboard error: {e}")));
                }
            },
            Err(e) => {
                self.row_messages.insert(id, (true, e.to_string()));
            }
        }
        cx.notify();
    }
}

impl Render for McpView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let card_bg = theme.group_box;
        let border = theme.border;
        let muted_fg = theme.muted_foreground;
        let danger_fg = theme.danger;
        let radius_lg = theme.radius_lg;

        let server_row = |label: &'static str, value: &'static str| {
            h_flex()
                .w_full()
                .justify_between()
                .items_center()
                .gap_4()
                .child(
                    div()
                        .text_sm()
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .child(label),
                )
                .child(div().text_sm().text_color(muted_fg).child(value))
        };

        let busy_any = self.busy_id.is_some();

        let client_cards: Vec<_> = self
            .clients
            .iter()
            .enumerate()
            .map(|(idx, client)| {
                let id = client.id;
                let is_file = matches!(client.kind, ClientKind::File { .. });
                let show_add = is_file
                    && client.detected
                    && matches!(
                        client.state,
                        EntryState::Absent | EntryState::Present { matches: false, .. }
                    );
                let show_remove = is_file && matches!(client.state, EntryState::Present { .. });
                let detected_label = if client.detected {
                    "Detected"
                } else {
                    "Not found"
                };
                let state_label = entry_state_label(client);
                let inline_msg = self.row_messages.get(id).cloned();

                v_flex()
                    .id(("mcp-client-row", idx))
                    .w_full()
                    .p_4()
                    .gap_2()
                    .rounded(radius_lg)
                    .border_1()
                    .border_color(border)
                    .bg(card_bg)
                    .child(
                        h_flex()
                            .w_full()
                            .items_center()
                            .justify_between()
                            .gap_4()
                            .child(
                                v_flex()
                                    .flex_1()
                                    .min_w_0()
                                    .gap_0p5()
                                    .child(
                                        div()
                                            .text_sm()
                                            .font_weight(gpui::FontWeight::MEDIUM)
                                            .child(client.name),
                                    )
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(muted_fg)
                                            .child(format!("{detected_label} · {state_label}")),
                                    ),
                            )
                            .child(
                                h_flex()
                                    .gap_2()
                                    .items_center()
                                    .children(show_add.then(|| {
                                        Button::new(("mcp-add", idx))
                                            .primary()
                                            .disabled(busy_any)
                                            .label("Add")
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                this.run_add(id, cx);
                                            }))
                                    }))
                                    .children(show_remove.then(|| {
                                        Button::new(("mcp-remove", idx))
                                            .danger()
                                            .disabled(busy_any)
                                            .label("Remove")
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                this.run_remove(id, cx);
                                            }))
                                    }))
                                    .child(
                                        Button::new(("mcp-copy", idx))
                                            .secondary()
                                            .label("Copy config")
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                this.copy_config(id, cx);
                                            })),
                                    ),
                            ),
                    )
                    .children(inline_msg.map(|(is_err, msg)| {
                        div()
                            .text_xs()
                            .text_color(if is_err { danger_fg } else { muted_fg })
                            .child(msg)
                    }))
            })
            .collect();

        div()
            .id("mcp-scroll")
            .size_full()
            .overflow_y_scroll()
            .p_6()
            .on_key_down(cx.listener(|this, ev: &KeyDownEvent, _, cx| {
                if ev.keystroke.key == "escape" && this.tip_visible {
                    this.tip_visible = false;
                    cx.notify();
                }
            }))
            .child(
                v_flex()
                    .max_w(px(860.0))
                    .gap_4()
                    .children(self.tip_visible.then(|| {
                        render_tip(
                            "mcp-tip-dismiss",
                            "MCP clients",
                            "Connect an AI app to your history. Nothing is changed until you press Add.",
                            cx.listener(|this, _, _, cx| {
                                this.tip_visible = false;
                                cx.notify();
                            }),
                            cx,
                        )
                    }))
                    .child(
                        div()
                            .text_xl()
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .child("MCP"),
                    )
                    .child(
                        v_flex()
                            .p_4()
                            .gap_2()
                            .rounded(radius_lg)
                            .border_1()
                            .border_color(border)
                            .bg(card_bg)
                            .child(
                                div()
                                    .text_sm()
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .child("Server status"),
                            )
                            .child(server_row("MCP server", "On"))
                            .child(server_row("Agent pipe", "Running"))
                            .child(server_row("Relay", MOCK_RELAY_PATH)),
                    )
                    .child(
                        div()
                            .text_sm()
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .child("Clients"),
                    )
                    .children(client_cards),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn make_temp_dir(label: &str) -> PathBuf {
        let base = std::env::var("BR_TEST_TMP")
            .map(PathBuf::from)
            .unwrap_or_else(|_| std::env::temp_dir());
        let seq = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = base.join(format!("br-ui-mcp-{label}-{}-{seq}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        dir
    }

    #[test]
    fn fake_backend_add_remove_cycle() {
        let backend = FakeMcpBackend::new(MOCK_RELAY_PATH);
        let initial = backend.list();
        assert_eq!(initial.len(), 3);
        let claude = initial.iter().find(|c| c.id == "claude-desktop").unwrap();
        assert_eq!(claude.state, EntryState::Absent);

        let added = backend.add("claude-desktop").expect("add claude-desktop");
        assert_eq!(added.kind, ChangeKind::Added);
        let after_add = backend.list();
        let claude = after_add.iter().find(|c| c.id == "claude-desktop").unwrap();
        assert!(matches!(
            claude.state,
            EntryState::Present { matches: true, .. }
        ));

        let removed = backend
            .remove("claude-desktop")
            .expect("remove claude-desktop");
        assert_eq!(removed.kind, ChangeKind::Removed);
        let after_remove = backend.list();
        let claude = after_remove
            .iter()
            .find(|c| c.id == "claude-desktop")
            .unwrap();
        assert_eq!(claude.state, EntryState::Absent);

        let snip = backend.snippet("claude-code").expect("snippet");
        assert!(snip.text.contains("betterrecall"));
    }

    #[test]
    fn real_backend_adds_and_removes_json_client_in_temp_env() {
        let root = make_temp_dir("real");
        let home = root.join("home");
        let appdata = root.join("appdata");
        let claude_dir = appdata.join("Claude");
        std::fs::create_dir_all(&home).expect("create home");
        std::fs::create_dir_all(&claude_dir).expect("create Claude dir");

        let relay = PathBuf::from(r"K:\scratch\fake-relay\betterrecall-mcp.exe");
        let env = ClientEnv {
            home,
            appdata,
            xdg_config_home: None,
            codex_home: None,
        };
        let backend = RealMcpBackend::new(env, relay);

        let added = backend.add("claude-desktop").expect("add claude-desktop");
        assert_eq!(added.kind, ChangeKind::Added);
        let cfg_path = claude_dir.join("claude_desktop_config.json");
        let contents_after_add = std::fs::read_to_string(&cfg_path).expect("read config");
        assert!(contents_after_add.contains("betterrecall"));
        assert!(contents_after_add.contains("betterrecall-mcp.exe"));

        let removed = backend
            .remove("claude-desktop")
            .expect("remove claude-desktop");
        assert_eq!(removed.kind, ChangeKind::Removed);
        let contents_after_remove =
            std::fs::read_to_string(&cfg_path).expect("read config after remove");
        assert!(!contents_after_remove.contains("betterrecall"));

        let _ = std::fs::remove_dir_all(&root);
    }
}
