// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

use gpui::{
    AppContext as _, Context, Entity, FocusHandle, InteractiveElement as _, IntoElement, ParentElement as _,
    Render, Styled as _, Window, div, prelude::FluentBuilder as _, px,
};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::{ActiveTheme as _, TitleBar, h_flex, v_flex};

use crate::args::{LaunchArgs, Page};
use crate::motion;
use crate::theme::NativeThemeInfo;
use crate::views::player::PlayerView;
use crate::views::settings_demo::SettingsDemoView;

pub struct MainShell {
    active_page: Page,
    settings_view: Entity<SettingsDemoView>,
    player_view: Entity<PlayerView>,
    capturable: bool,
    /// Keeps the window's focus path inside Root, so Tab / Shift+Tab reach the first tab stop.
    focus_handle: FocusHandle,
}

impl MainShell {
    pub fn new(args: &LaunchArgs, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let settings_view = cx.new(|cx| SettingsDemoView::new(window, cx));
        let player_view = cx.new(|cx| {
            PlayerView::new(args.video.clone(), args.offset_ms, args.stats, window, cx)
        });
        let focus_handle = cx.focus_handle();
        if args.page == Page::Player {
            let h = player_view.read(cx).focus_handle();
            window.focus(&h, cx);
        } else {
            window.focus(&focus_handle, cx);
        }
        Self {
            active_page: args.page,
            settings_view,
            player_view,
            capturable: args.mock_capturable,
            focus_handle,
        }
    }

    fn go(&mut self, page: Page, window: &mut Window, cx: &mut Context<Self>) {
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

        let nav_item = |id: &'static str, label: &'static str, page: Page| {
            let is_active = active == page;
            Button::new(id)
                .label(label)
                .w_full()
                .justify_start()
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
                            .child(nav_item("nav-player", "Player", Page::Player)),
                    )
                    .child(
                        div()
                            .id("shell-content")
                            .flex_1()
                            .h_full()
                            .min_w_0()
                            .min_h_0()
                            .overflow_hidden()
                            .map(|el| match self.active_page {
                                Page::Home => el.child(self.render_home(cx)),
                                Page::Settings => el.child(self.settings_view.clone()),
                                Page::Player => el.child(self.player_view.clone()),
                            }),
                    ),
            )
    }
}
