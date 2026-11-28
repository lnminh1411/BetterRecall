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
    AppContext as _, Context, FocusHandle, InteractiveElement as _, IntoElement, KeyDownEvent,
    ParentElement as _, Render, StatefulInteractiveElement as _, Styled as _, Window, div, px,
};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::{ActiveTheme as _, Disableable as _, h_flex, v_flex};

use crate::platform;
use crate::views::tip::render_tip;

pub const MOCK_RELAY_PATH: &str = r"%LOCALAPPDATA%\BetterRecall\app\bin\betterrecall-mcp.exe";

fn detect_zed(env: &ClientEnv) -> bool {
    let xdg_zed = env
        .xdg_config_home
        .as_deref()
        .map(|p| p.join("zed"))
        .unwrap_or_else(|| env.home.join(".config").join("zed"));
    env.appdata.join("Zed").exists() || xdg_zed.exists()
}

fn detect_cline(env: &ClientEnv) -> bool {
    env.appdata
        .join("Code")
        .join("User")
        .join("globalStorage")
        .join("saoudrizwan.claude-dev")
        .exists()
        || env
            .appdata
            .join("Code - Insiders")
            .join("User")
            .join("globalStorage")
            .join("saoudrizwan.claude-dev")
            .exists()
        || env.home.join(".cline").exists()
}

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
        let mut statuses = br_mcp_clients::list(&self.env, &self.relay, None);
        for status in &mut statuses {
            if !status.detected {
                match status.id {
                    "zed" => status.detected = detect_zed(&self.env),
                    "cline" => status.detected = detect_cline(&self.env),
                    _ => {}
                }
            }
        }
        statuses
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
        let _client = guard
            .iter()
            .find(|c| c.id == id)
            .ok_or_else(|| ClientError::UnknownClient(id.to_string()))?;
        drop(guard);

        let fake_env = ClientEnv {
            home: PathBuf::from("%USERPROFILE%"),
            appdata: PathBuf::from("%APPDATA%"),
            xdg_config_home: None,
            codex_home: None,
        };
        br_mcp_clients::snippet(&fake_env, id, Path::new(&self.relay), None)
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

fn detected_status_label(client: &ClientStatus) -> &'static str {
    if client.id == "other" {
        "Manual"
    } else if client.detected {
        "Detected"
    } else {
        "Not found"
    }
}

fn add_or_update_label(client: &ClientStatus) -> Option<&'static str> {
    if !matches!(client.kind, ClientKind::File { .. }) || !client.detected {
        return None;
    }
    match client.state {
        EntryState::Absent => Some("Add"),
        EntryState::Present { matches: false, .. } => Some("Update"),
        _ => None,
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

fn collect_clients_and_hints(
    backend: &dyn McpBackend,
) -> (Vec<ClientStatus>, HashMap<&'static str, String>) {
    let clients = backend.list();
    let mut hints = HashMap::with_capacity(clients.len());
    for client in &clients {
        if let Ok(snip) = backend.snippet(client.id)
            && !snip.target_hint.is_empty()
        {
            hints.insert(client.id, snip.target_hint);
        }
    }
    (clients, hints)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum BusyAction {
    Primary,
    Remove,
}

pub struct McpView {
    backend: Arc<dyn McpBackend>,
    clients: Vec<ClientStatus>,
    target_hints: HashMap<&'static str, String>,
    row_messages: HashMap<&'static str, (bool, String)>,
    busy_action: Option<(&'static str, BusyAction)>,
    refresh_gen: u64,
    tip_visible: bool,
    focus_handle: FocusHandle,
}

impl McpView {
    pub fn new(mock_mcp_home: Option<&Path>, show_tip: bool, cx: &mut Context<Self>) -> Self {
        let backend = build_mock_backend(mock_mcp_home);
        let (clients, target_hints) = collect_clients_and_hints(backend.as_ref());
        let focus_handle = cx.focus_handle();
        Self {
            backend,
            clients,
            target_hints,
            row_messages: HashMap::new(),
            busy_action: None,
            refresh_gen: 0,
            tip_visible: show_tip,
            focus_handle,
        }
    }

    pub fn focus_handle(&self) -> FocusHandle {
        self.focus_handle.clone()
    }

    /// Re-read the client list whenever the MCP page is opened so external edits are reflected.
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        if self.busy_action.is_some() {
            return;
        }
        self.refresh_gen = self.refresh_gen.wrapping_add(1);
        let epoch = self.refresh_gen;
        let backend = self.backend.clone();
        cx.spawn(async move |weak, cx| {
            let (clients, hints) = cx
                .background_spawn(async move { collect_clients_and_hints(backend.as_ref()) })
                .await;
            let _ = weak.update(cx, |this, cx| {
                if this.refresh_gen == epoch && this.busy_action.is_none() {
                    this.clients = clients;
                    this.target_hints = hints;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn run_add(&mut self, id: &'static str, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy_action.is_some() {
            return;
        }
        self.refresh_gen = self.refresh_gen.wrapping_add(1);
        self.busy_action = Some((id, BusyAction::Primary));
        cx.notify();

        let backend = self.backend.clone();
        cx.spawn_in(window, async move |weak, cx| {
            let (res, refreshed, hints) = cx
                .background_spawn(async move {
                    let res = backend.add(id);
                    let (refreshed, hints) = collect_clients_and_hints(backend.as_ref());
                    (res, refreshed, hints)
                })
                .await;

            let _ = weak.update_in(cx, |this, window, cx| {
                this.clients = refreshed;
                this.target_hints = hints;
                this.busy_action = None;
                match res {
                    Ok(change) => {
                        this.row_messages
                            .insert(id, (false, format_change_message(&change)));
                    }
                    Err(e) => {
                        this.row_messages.insert(id, (true, e.to_string()));
                    }
                }
                // If the primary button slot no longer exists for this row, move focus to the page.
                let has_primary = this.clients.iter().find(|c| c.id == id).is_some_and(|c| {
                    add_or_update_label(c).is_some()
                        || (matches!(c.kind, ClientKind::File { .. })
                            && matches!(c.state, EntryState::Present { matches: true, .. }))
                });
                if !has_primary {
                    window.focus(&this.focus_handle, cx);
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn run_remove(
        &mut self,
        id: &'static str,
        slot: BusyAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.busy_action.is_some() {
            return;
        }
        self.refresh_gen = self.refresh_gen.wrapping_add(1);
        self.busy_action = Some((id, slot));
        cx.notify();

        let backend = self.backend.clone();
        cx.spawn_in(window, async move |weak, cx| {
            let (res, refreshed, hints) = cx
                .background_spawn(async move {
                    let res = backend.remove(id);
                    let (refreshed, hints) = collect_clients_and_hints(backend.as_ref());
                    (res, refreshed, hints)
                })
                .await;

            let _ = weak.update_in(cx, |this, window, cx| {
                this.clients = refreshed;
                this.target_hints = hints;
                this.busy_action = None;
                match res {
                    Ok(change) => {
                        this.row_messages
                            .insert(id, (false, format_change_message(&change)));
                    }
                    Err(e) => {
                        this.row_messages.insert(id, (true, e.to_string()));
                    }
                }
                // If the clicked secondary Remove button disappeared (or primary slot is gone),
                // move focus to the page handle so keyboard users keep a focus position.
                let has_primary = this.clients.iter().find(|c| c.id == id).is_some_and(|c| {
                    add_or_update_label(c).is_some()
                        || (matches!(c.kind, ClientKind::File { .. })
                            && matches!(c.state, EntryState::Present { matches: true, .. }))
                });
                if slot == BusyAction::Remove || !has_primary {
                    window.focus(&this.focus_handle, cx);
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

        let busy_action = self.busy_action;
        let busy_any = busy_action.is_some();

        let client_cards: Vec<_> = self
            .clients
            .iter()
            .enumerate()
            .map(|(idx, client)| {
                let id = client.id;
                let is_file = matches!(client.kind, ClientKind::File { .. });
                let add_label = add_or_update_label(client);
                let is_present = is_file && matches!(client.state, EntryState::Present { .. });
                // When only Add/Update OR only Remove is shown, both reuse the same primary id
                // slot ("mcp-primary", idx) so keyboard focus stays on the replacing button.
                let primary_is_remove = add_label.is_none() && is_present;
                let show_secondary_remove = add_label.is_some() && is_present;
                let detected_label = detected_status_label(client);
                let state_label = entry_state_label(client);
                let target_hint = self.target_hints.get(id).cloned();
                let inline_msg = self.row_messages.get(id).cloned();

                let primary_loading = busy_action == Some((id, BusyAction::Primary));
                let primary_disabled = busy_any && !primary_loading;
                let remove_loading = busy_action == Some((id, BusyAction::Remove));
                let remove_disabled = busy_any && !remove_loading;

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
                                        h_flex()
                                            .gap_2()
                                            .items_center()
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
                                                    .child(format!(
                                                        "{detected_label} · {state_label}"
                                                    )),
                                            ),
                                    )
                                    .children(target_hint.map(|hint| {
                                        div()
                                            .text_xs()
                                            .text_color(muted_fg)
                                            .truncate()
                                            .child(hint)
                                    })),
                            )
                            .child(
                                h_flex()
                                    .gap_2()
                                    .items_center()
                                    .children(add_label.map(|label| {
                                        Button::new(("mcp-primary", idx))
                                            .primary()
                                            .loading(primary_loading)
                                            .disabled(primary_disabled)
                                            .label(label)
                                            .on_click(cx.listener(move |this, _, window, cx| {
                                                this.run_add(id, window, cx);
                                            }))
                                    }))
                                    .children(primary_is_remove.then(|| {
                                        Button::new(("mcp-primary", idx))
                                            .danger()
                                            .loading(primary_loading)
                                            .disabled(primary_disabled)
                                            .label("Remove")
                                            .on_click(cx.listener(move |this, _, window, cx| {
                                                this.run_remove(
                                                    id,
                                                    BusyAction::Primary,
                                                    window,
                                                    cx,
                                                );
                                            }))
                                    }))
                                    .children(show_secondary_remove.then(|| {
                                        Button::new(("mcp-remove", idx))
                                            .danger()
                                            .loading(remove_loading)
                                            .disabled(remove_disabled)
                                            .label("Remove")
                                            .on_click(cx.listener(move |this, _, window, cx| {
                                                this.run_remove(
                                                    id,
                                                    BusyAction::Remove,
                                                    window,
                                                    cx,
                                                );
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
            .track_focus(&self.focus_handle)
            .size_full()
            .overflow_y_scroll()
            .p_6()
            .on_key_down(cx.listener(|this, ev: &KeyDownEvent, window, cx| {
                if ev.keystroke.key == "escape" && this.tip_visible {
                    this.tip_visible = false;
                    window.focus(&this.focus_handle, cx);
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
                            cx.listener(|this, _, window, cx| {
                                this.tip_visible = false;
                                window.focus(&this.focus_handle, cx);
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

    #[test]
    fn detects_zed_and_cline_in_mock_home_and_marks_other_manual() {
        let root = make_temp_dir("zed-cline");
        let home = root.join("home");
        let appdata = root.join("appdata");
        std::fs::create_dir_all(&home).expect("create home");
        std::fs::create_dir_all(&appdata).expect("create appdata");

        let relay = PathBuf::from(MOCK_RELAY_PATH);
        let backend = RealMcpBackend::from_mock_home(&root, relay);

        let before = backend.list();
        assert!(!before.iter().find(|c| c.id == "zed").unwrap().detected);
        assert!(!before.iter().find(|c| c.id == "cline").unwrap().detected);
        let other = before.iter().find(|c| c.id == "other").unwrap();
        assert_eq!(detected_status_label(other), "Manual");

        std::fs::create_dir_all(appdata.join("Zed")).expect("create Zed dir");
        std::fs::create_dir_all(
            appdata
                .join("Code")
                .join("User")
                .join("globalStorage")
                .join("saoudrizwan.claude-dev"),
        )
        .expect("create Cline dir");

        let after = backend.list();
        let zed = after.iter().find(|c| c.id == "zed").unwrap();
        let cline = after.iter().find(|c| c.id == "cline").unwrap();
        assert!(zed.detected);
        assert!(cline.detected);
        assert_eq!(detected_status_label(zed), "Detected");
        assert_eq!(detected_status_label(cline), "Detected");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn mismatched_entry_shows_update_label() {
        let absent = ClientStatus {
            id: "cursor",
            name: "Cursor",
            kind: ClientKind::File {
                path: PathBuf::from("mcp.json"),
                format: ConfigFormat::Json,
            },
            detected: true,
            state: EntryState::Absent,
        };
        assert_eq!(add_or_update_label(&absent), Some("Add"));

        let mismatched = ClientStatus {
            state: EntryState::Present {
                command: Some("old.exe".into()),
                matches: false,
            },
            ..absent.clone()
        };
        assert_eq!(add_or_update_label(&mismatched), Some("Update"));

        let matched = ClientStatus {
            state: EntryState::Present {
                command: Some(MOCK_RELAY_PATH.into()),
                matches: true,
            },
            ..absent
        };
        assert_eq!(add_or_update_label(&matched), None);
    }

    #[test]
    fn fake_claude_code_snippet_matches_br_mcp_clients_exactly() {
        let backend = FakeMcpBackend::new(MOCK_RELAY_PATH);
        let fake_snip = backend.snippet("claude-code").expect("fake snippet");
        let env = ClientEnv {
            home: PathBuf::from("%USERPROFILE%"),
            appdata: PathBuf::from("%APPDATA%"),
            xdg_config_home: None,
            codex_home: None,
        };
        let real_snip = br_mcp_clients::snippet(&env, "claude-code", Path::new(MOCK_RELAY_PATH), None)
            .expect("real snippet");
        assert_eq!(fake_snip.text, real_snip.text);
        assert_eq!(fake_snip.command, real_snip.command);
        assert_eq!(fake_snip.target_hint, real_snip.target_hint);
    }
}
