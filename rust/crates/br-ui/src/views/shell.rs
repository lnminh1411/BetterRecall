// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

use std::path::PathBuf;

use gpui::{
    AppContext as _, Context, Entity, FocusHandle, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, Styled as _, Subscription, Window, div,
    prelude::FluentBuilder as _, px,
};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::{ActiveTheme as _, Disableable as _, TitleBar, h_flex, v_flex};

use crate::args::{LaunchArgs, Page};
use crate::links;
use crate::motion;
use crate::theme::NativeThemeInfo;
use crate::views::about::AboutView;
use crate::views::first_run::{FirstRunEvent, FirstRunView};
use crate::views::help::{HelpEvent, HelpView};
use crate::views::mcp::McpView;
use crate::views::player::PlayerView;
use crate::views::settings_demo::SettingsDemoView;

pub struct MainShell {
    active_page: Page,
    show_first_run: bool,
    settings_view: Entity<SettingsDemoView>,
    player_view: Entity<PlayerView>,
    mcp_view: Entity<McpView>,
    about_view: Entity<AboutView>,
    help_view: Entity<HelpView>,
    first_run_view: Entity<FirstRunView>,
    capturable: bool,
    no_shell: bool,
    days_used: u32,
    support_dismissed: bool,
    support_status: Option<String>,
    /// Keeps the window's focus path inside Root, so Tab / Shift+Tab reach the first tab stop.
    focus_handle: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

impl MainShell {
    pub fn new(
        args: &LaunchArgs,
        assets_dir: Option<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let settings_view = cx.new(|cx| SettingsDemoView::new(window, cx));
        let player_view = cx.new(|cx| {
            PlayerView::new(
                args.video.clone(),
                args.offset_ms,
                args.stats,
                args.mock_tips,
                window,
                cx,
            )
        });
        let mcp_home = args.mock_mcp_home.clone();
        let show_tips = args.mock_tips;
        let mcp_view = cx.new(|_| McpView::new(mcp_home.as_deref(), show_tips));
        let no_shell = args.mock_no_shell;
        let about_view = cx.new(|_| AboutView::new(no_shell));
        let help_view = cx.new(|_| HelpView::new(no_shell));
        let first_run_view = cx.new(|cx| FirstRunView::new(assets_dir, window, cx));

        let mut subscriptions = Vec::new();
        subscriptions.push(cx.subscribe(
            &first_run_view,
            |this, _, _ev: &FirstRunEvent, cx| {
                this.show_first_run = false;
                this.active_page = Page::Home;
                cx.notify();
            },
        ));
        subscriptions.push(cx.subscribe_in(
            &help_view,
            window,
            |this, _, ev: &HelpEvent, window, cx| match ev {
                HelpEvent::ShowIntroduction => {
                    this.first_run_view.update(cx, |v, cx| v.reset(window, cx));
                    this.show_first_run = true;
                    cx.notify();
                }
            },
        ));

        let focus_handle = cx.focus_handle();
        if !args.mock_firstrun && args.page == Page::Player {
            let h = player_view.read(cx).focus_handle();
            window.focus(&h, cx);
        } else {
            window.focus(&focus_handle, cx);
        }

        Self {
            active_page: args.page,
            show_first_run: args.mock_firstrun,
            settings_view,
            player_view,
            mcp_view,
            about_view,
            help_view,
            first_run_view,
            capturable: args.mock_capturable,
            no_shell,
            days_used: args.mock_days_used,
            support_dismissed: false,
            support_status: None,
            focus_handle,
            _subscriptions: subscriptions,
        }
    }

    fn go(&mut self, page: Page, window: &mut Window, cx: &mut Context<Self>) {
        if self.show_first_run {
            return;
        }
        self.active_page = page;
        if page == Page::Player {
            let h = self.player_view.read(cx).focus_handle();
            window.focus(&h, cx);
        }
        cx.notify();
    }

    fn render_home(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let info = cx.try_global::<NativeThemeInfo>().cloned();
        let mode_label = match info.as_ref() {
            Some(i) if i.high_contrast => "High Contrast",
            Some(i) if i.is_dark => "Dark",
            Some(_) => "Light",
            None => "Unknown",
        };
        let font_label = info
            .map(|i| i.font_family.to_string())
            .unwrap_or_else(|| "Segoe UI Variable".to_string());
        let motion_label = format!(
            "{} (reduce_motion={})",
            motion::current_mode(cx).as_str(),
            motion::is_reduced(cx)
        );
        let affinity_label = if self.capturable {
            "Capturable (--mock-capturable)"
        } else {
            "Excluded from capture (WDA_EXCLUDEFROMCAPTURE)"
        };
        let show_support = links::should_show_support_card(self.days_used, self.support_dismissed);

        v_flex()
            .size_full()
            .p_6()
            .gap_4()
            .child(
                div()
                    .text_xl()
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .child("BetterRecall GPUI-CE Spike"),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child("Mock mode spike validating GPUI-CE 0.3.0, native-theme 0.7.2, keyboard navigation, Win32 window chrome, and Media Foundation + WASAPI playback."),
            )
            .children(show_support.then(|| {
                v_flex()
                    .p_4()
                    .gap_2()
                    .rounded(theme.radius_lg)
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.group_box)
                    .child(
                        div()
                            .text_sm()
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .child("Support development"),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child("BetterRecall is free and open source. If it has been useful to you, consider supporting its development."),
                    )
                    .child(
                        h_flex()
                            .pt_1()
                            .gap_2()
                            .items_center()
                            .child(
                                Button::new("home-support-open")
                                    .primary()
                                    .label("Support")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        if let Err(e) =
                                            links::open_url(links::SUPPORT, this.no_shell)
                                        {
                                            this.support_status =
                                                Some(format!("Could not open link: {e}"));
                                            cx.notify();
                                        }
                                    })),
                            )
                            .child(
                                Button::new("home-support-dismiss")
                                    .secondary()
                                    .label("Not now")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        // Session-only dismissal; persistence comes with the real settings store.
                                        this.support_dismissed = true;
                                        cx.notify();
                                    })),
                            )
                            .children(self.support_status.clone().map(|msg| {
                                div().text_xs().text_color(theme.danger).child(msg)
                            })),
                    )
            }))
            .child(
                v_flex()
                    .p_4()
                    .gap_2()
                    .rounded(theme.radius_lg)
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.group_box)
                    .child(div().text_sm().font_weight(gpui::FontWeight::SEMIBOLD).child("Runtime status"))
                    .child(div().text_sm().child(format!("Theme: {mode_label}")))
                    .child(div().text_sm().child(format!("UI font: {font_label}")))
                    .child(div().text_sm().child(format!("Motion: {motion_label}")))
                    .child(div().text_sm().child(format!("Display affinity: {affinity_label}")))
                    .child(
                        h_flex()
                            .pt_2()
                            .gap_2()
                            .child(
                                Button::new("home-open-settings")
                                    .primary()
                                    .label("Open Settings Demo")
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.go(Page::Settings, window, cx);
                                    })),
                            )
                            .child(
                                Button::new("home-open-player")
                                    .secondary()
                                    .label("Open Replay Player")
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.go(Page::Player, window, cx);
                                    })),
                            ),
                    ),
            )
    }
}

impl Render for MainShell {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let active = self.active_page;
        let show_first_run = self.show_first_run;

        let nav_item = |id: &'static str, label: &'static str, page: Page| {
            let is_active = !show_first_run && active == page;
            Button::new(id)
                .label(label)
                .w_full()
                .justify_start()
                .disabled(show_first_run)
                .when(is_active, |b| b.primary())
                .when(!is_active, |b| b.ghost())
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.go(page, window, cx);
                }))
        };

        v_flex()
            .id("shell-root")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(theme.background)
            .text_color(theme.foreground)
            .child(
                TitleBar::new().child(
                    h_flex()
                        .items_center()
                        .gap_2()
                        .child(
                            div()
                                .text_sm()
                                .font_weight(gpui::FontWeight::SEMIBOLD)
                                .child("BetterRecall"),
                        ),
                ),
            )
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .child(
                        v_flex()
                            .w(px(200.0))
                            .h_full()
                            .p_3()
                            .gap_1()
                            .bg(theme.sidebar)
                            .border_r_1()
                            .border_color(theme.sidebar_border)
                            .child(nav_item("nav-home", "Home", Page::Home))
                            .child(nav_item("nav-settings", "Settings", Page::Settings))
                            .child(nav_item("nav-player", "Player", Page::Player))
                            .child(nav_item("nav-mcp", "MCP", Page::Mcp))
                            .child(nav_item("nav-about", "About", Page::About))
                            .child(nav_item("nav-help", "Help", Page::Help)),
                    )
                    .child(
                        div()
                            .id("shell-content")
                            .flex_1()
                            .h_full()
                            .min_w_0()
                            .min_h_0()
                            .overflow_hidden()
                            .map(|el| {
                                if self.show_first_run {
                                    el.child(self.first_run_view.clone())
                                } else {
                                    match self.active_page {
                                        Page::Home => el.child(self.render_home(cx)),
                                        Page::Settings => el.child(self.settings_view.clone()),
                                        Page::Player => el.child(self.player_view.clone()),
                                        Page::Mcp => el.child(self.mcp_view.clone()),
                                        Page::About => el.child(self.about_view.clone()),
                                        Page::Help => el.child(self.help_view.clone()),
                                    }
                                }
                            }),
                    ),
            )
    }
}
