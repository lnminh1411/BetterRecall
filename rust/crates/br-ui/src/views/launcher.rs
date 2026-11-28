// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

use std::path::PathBuf;

use gpui::{
    Context, Entity, Focusable as _, InteractiveElement as _, IntoElement, KeyDownEvent, ObjectFit,
    ParentElement as _, Render, SharedString, Styled as _, Subscription, Window, div, img, px,
};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::select::{SearchableVec, Select, SelectState};
use gpui_component::{ActiveTheme as _, IndexPath, h_flex, v_flex};

use crate::platform;

const DEMO_CARDS: [(&str, &str, &str); 9] = [
    ("mail-invoice.png", "Invoice #4821 — Acme Cloud", "Outlook"),
    ("chat-design-review.png", "Design Review Thread", "Teams"),
    ("code-agentclient.png", "AgentClient.cs — Pipe Client", "VS Code"),
    ("docs-navview.png", "WinUI NavigationView Spec", "Edge"),
    ("excel-budget.png", "Q4 Budget Forecast.xlsx", "Excel"),
    ("flights-han-nrt.png", "HAN -> NRT Flight Options", "Edge"),
    ("pr-replay-export.png", "PR #142: Replay Export", "Edge"),
    ("term-git-log.png", "git log --oneline -20", "Terminal"),
    ("word-meeting-notes.png", "Weekly Sync Notes.docx", "Word"),
];

pub struct LauncherView {
    search_input: Entity<InputState>,
    app_filter: Entity<SelectState<SearchableVec<SharedString>>>,
    time_filter: Entity<SelectState<SearchableVec<SharedString>>>,
    kind_filter: Entity<SelectState<SearchableVec<SharedString>>>,
    assets_dir: Option<PathBuf>,
    query_text: String,
    clipboard_status: Option<String>,
    _subscriptions: Vec<Subscription>,
}

impl LauncherView {
    pub fn new(
        initial_query: Option<String>,
        assets_dir: Option<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let query_str = initial_query.unwrap_or_default();
        let search_input = cx.new(|cx| {
            let mut st = InputState::new(window, cx).placeholder("Search your recall history...");
            if !query_str.is_empty() {
                st.set_value(query_str.clone(), window, cx);
            }
            st
        });

        let app_filter = cx.new(|cx| {
            SelectState::new(
                SearchableVec::new(vec![
                    "All apps".into(),
                    "Edge".into(),
                    "VS Code".into(),
                    "Outlook".into(),
                    "Teams".into(),
                ]),
                Some(IndexPath::default()),
                window,
                cx,
            )
        });

        let time_filter = cx.new(|cx| {
            SelectState::new(
                SearchableVec::new(vec![
                    "Any time".into(),
                    "Today".into(),
                    "Past 7 days".into(),
                    "Past 30 days".into(),
                ]),
                Some(IndexPath::default()),
                window,
                cx,
            )
        });

        let kind_filter = cx.new(|cx| {
            SelectState::new(
                SearchableVec::new(vec![
                    "All kinds".into(),
                    "Screen".into(),
                    "Audio".into(),
                    "Replay".into(),
                ]),
                Some(IndexPath::default()),
                window,
                cx,
            )
        });

        let mut subscriptions = Vec::new();

        subscriptions.push(cx.subscribe_in(
            &search_input,
            window,
            |this, state, ev: &InputEvent, _window, cx| {
                if matches!(ev, InputEvent::Change) {
                    this.query_text = state.read(cx).value().to_string();
                    cx.notify();
                }
            },
        ));

        // Hide the launcher whenever it loses window focus (deactivates).
        subscriptions.push(cx.observe_window_activation(window, |_this, window, _cx| {
            if !window.is_window_active() {
                window.set_visible(false);
            }
        }));

        Self {
            search_input,
            app_filter,
            time_filter,
            kind_filter,
            assets_dir,
            query_text: query_str,
            clipboard_status: None,
            _subscriptions: subscriptions,
        }
    }

    pub fn show_with_query(&mut self, query: Option<&str>, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(q) = query {
            let owned = q.to_string();
            self.query_text = owned.clone();
            self.search_input.update(cx, |input, cx| {
                input.set_value(owned, window, cx);
            });
        }
        let handle = self.search_input.read(cx).focus_handle(cx);
        window.focus(&handle, cx);
        cx.notify();
    }

    pub fn focus_search(&self, window: &mut Window, cx: &mut App) {
        let handle = self.search_input.read(cx).focus_handle(cx);
        window.focus(&handle, cx);
    }
}

use gpui::App;

impl Render for LauncherView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let query_lower = self.query_text.trim().to_ascii_lowercase();

        let cards: Vec<_> = DEMO_CARDS
            .iter()
            .enumerate()
            .filter(|(_, (_, title, app))| {
                query_lower.is_empty()
                    || title.to_ascii_lowercase().contains(&query_lower)
                    || app.to_ascii_lowercase().contains(&query_lower)
            })
            .collect();

        v_flex()
            .size_full()
            .p_4()
            .gap_3()
            .bg(theme.background)
            .text_color(theme.foreground)
            .border_1()
            .border_color(theme.border)
            .rounded(theme.radius_lg)
            .on_key_down(|ev: &KeyDownEvent, window, _cx| {
                if ev.keystroke.key == "escape" {
                    window.set_visible(false);
                }
            })
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        div().flex_1().child(
                            Input::new(&self.search_input)
                                .aria_label("Search history")
                                .cleanable(true),
                        ),
                    )
                    .child(
                        Button::new("launcher-copy-text")
                            .secondary()
                            .label("Copy text")
                            .on_click(cx.listener(|this, _, _, cx| {
                                let text = this.search_input.read(cx).value().to_string();
                                let to_copy = if text.is_empty() {
                                    "BetterRecall spike test text".to_string()
                                } else {
                                    text
                                };
                                match platform::clipboard::copy_text_excluded(&to_copy) {
                                    Ok(()) => {
                                        this.clipboard_status =
                                            Some("Copied (excluded from history)".into());
                                    }
                                    Err(e) => {
                                        this.clipboard_status = Some(format!("Copy failed: {e}"));
                                    }
                                }
                                cx.notify();
                            })),
                    ),
            )
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        div().w(px(160.0)).child(
                            Select::new(&self.app_filter)
                                .accessibility_label("Filter by application")
                                .placeholder("App"),
                        ),
                    )
                    .child(
                        div().w(px(160.0)).child(
                            Select::new(&self.time_filter)
                                .accessibility_label("Filter by time range")
                                .placeholder("Time"),
                        ),
                    )
                    .child(
                        div().w(px(160.0)).child(
                            Select::new(&self.kind_filter)
                                .accessibility_label("Filter by capture kind")
                                .placeholder("Kind"),
                        ),
                    )
                    .child(div().flex_1())
                    .children(self.clipboard_status.clone().map(|status| {
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(status)
                    })),
            )
            .child(
                div()
                    .id("launcher-grid")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(
                        div()
                            .grid()
                            .grid_cols(3)
                            .gap_3()
                            .children(cards.into_iter().map(|(idx, (file, title, app))| {
                                let img_path = self.assets_dir.as_ref().map(|d| d.join(file));
                                v_flex()
                                    .id(("launcher-card", idx))
                                    .rounded(theme.radius_lg)
                                    .border_1()
                                    .border_color(theme.border)
                                    .bg(theme.group_box)
                                    .overflow_hidden()
                                    .child(match img_path {
                                        Some(p) if p.is_file() => img(p)
                                            .w_full()
                                            .h(px(150.0))
                                            .object_fit(ObjectFit::Cover)
                                            .into_any_element(),
                                        _ => div()
                                            .w_full()
                                            .h(px(150.0))
                                            .bg(theme.muted)
                                            .flex()
                                            .items_center()
                                            .justify_center()
                                            .text_xs()
                                            .text_color(theme.muted_foreground)
                                            .child(*file)
                                            .into_any_element(),
                                    })
                                    .child(
                                        v_flex()
                                            .p_2()
                                            .gap_0p5()
                                            .child(
                                                div()
                                                    .text_sm()
                                                    .font_weight(gpui::FontWeight::MEDIUM)
                                                    .child(*title),
                                            )
                                            .child(
                                                div()
                                                    .text_xs()
                                                    .text_color(theme.muted_foreground)
                                                    .child(*app),
                                            ),
                                    )
                            })),
                    ),
            )
    }
}
