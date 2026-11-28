// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

use gpui::{
    Context, InteractiveElement as _, IntoElement, ParentElement as _, Render,
    StatefulInteractiveElement as _, Styled as _, Window, div, px,
};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::{ActiveTheme as _, h_flex, v_flex};

use crate::links;
use crate::motion;
use crate::platform;
use crate::theme::NativeThemeInfo;

pub struct AboutView {
    no_shell: bool,
    windows_build: String,
    status_message: Option<(bool, String)>,
}

impl AboutView {
    pub fn new(no_shell: bool) -> Self {
        #[cfg(windows)]
        let windows_build = platform::shell::windows_build_string();
        #[cfg(not(windows))]
        let windows_build = "unknown".to_string();

        Self {
            no_shell,
            windows_build,
            status_message: None,
        }
    }

    fn open_link(&mut self, url: &str, cx: &mut Context<Self>) {
        match links::open_url(url, self.no_shell) {
            Ok(()) => {
                self.status_message = Some((
                    false,
                    if self.no_shell {
                        format!("Link logged (--mock-no-shell): {url}")
                    } else {
                        "Opened in your default browser.".to_string()
                    },
                ));
            }
            Err(e) => {
                self.status_message = Some((true, format!("Could not open link: {e}")));
            }
        }
        cx.notify();
    }

    fn copy_diagnostics(&mut self, cx: &mut Context<Self>) {
        let theme_mode = match cx.try_global::<NativeThemeInfo>() {
            Some(i) if i.high_contrast => "High Contrast",
            Some(i) if i.is_dark => "Dark",
            Some(_) => "Light",
            None => "Unknown",
        };
        let motion_mode = motion::current_mode(cx).as_str();
        let text = links::format_diagnostics(
            env!("CARGO_PKG_VERSION"),
            &self.windows_build,
            theme_mode,
            motion_mode,
        );

        let result = platform::clipboard::copy_text_excluded(&text);
        if self.no_shell {
            eprintln!("clipboard-diagnostics {} bytes", text.len());
        }
        match result {
            Ok(()) => {
                self.status_message = Some((
                    false,
                    "Copied diagnostics to clipboard (excluded from clipboard history)."
                        .to_string(),
                ));
            }
            Err(e) => {
                self.status_message = Some((true, format!("Clipboard error: {e}")));
            }
        }
        cx.notify();
    }
}

impl Render for AboutView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let card_bg = theme.group_box;
        let border = theme.border;
        let muted_fg = theme.muted_foreground;
        let danger_fg = theme.danger;
        let radius_lg = theme.radius_lg;

        let link_row = |id: &'static str,
                        title: &'static str,
                        subtitle: &'static str,
                        btn_label: &'static str,
                        url: String,
                        cx: &mut Context<Self>| {
            h_flex()
                .w_full()
                .p_4()
                .gap_4()
                .items_center()
                .justify_between()
                .rounded(radius_lg)
                .border_1()
                .border_color(border)
                .bg(card_bg)
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .gap_0p5()
                        .child(
                            div()
                                .text_sm()
                                .font_weight(gpui::FontWeight::MEDIUM)
                                .child(title),
                        )
                        .child(div().text_xs().text_color(muted_fg).child(subtitle)),
                )
                .child(
                    Button::new(id)
                        .secondary()
                        .label(btn_label)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.open_link(&url, cx);
                        })),
                )
        };

        let bug_report_url =
            links::build_bug_report_url(env!("CARGO_PKG_VERSION"), &self.windows_build);

        div()
            .id("about-scroll")
            .size_full()
            .overflow_y_scroll()
            .p_6()
            .child(
                v_flex()
                    .max_w(px(860.0))
                    .gap_4()
                    .child(
                        div()
                            .text_xl()
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .child("About BetterRecall"),
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
                                    .child(format!("Version {}", env!("CARGO_PKG_VERSION"))),
                            )
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(muted_fg)
                                    .child("Publisher: BetterRecall contributors"),
                            )
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(muted_fg)
                                    .child("Licence: AGPL-3.0-only"),
                            ),
                    )
                    .child(link_row(
                        "about-github",
                        "GitHub",
                        "Source code, releases, and issue tracker",
                        "Open",
                        links::GITHUB.to_string(),
                        cx,
                    ))
                    .child(link_row(
                        "about-models",
                        "Models on Hugging Face",
                        "Local OCR, speech, and embedding weights",
                        "Open",
                        links::MODELS.to_string(),
                        cx,
                    ))
                    .child(link_row(
                        "about-licences",
                        "Licences",
                        "AGPL-3.0-only and third-party notices",
                        "Open",
                        links::LICENCES.to_string(),
                        cx,
                    ))
                    .child(link_row(
                        "about-report-problem",
                        "Report a problem",
                        "Opens a pre-filled GitHub issue with version and Windows build only",
                        "Report a problem",
                        bug_report_url,
                        cx,
                    ))
                    .child(link_row(
                        "about-support",
                        "Support development",
                        "Help fund BetterRecall development",
                        "Support development",
                        links::SUPPORT.to_string(),
                        cx,
                    ))
                    .child(
                        h_flex()
                            .w_full()
                            .p_4()
                            .gap_4()
                            .items_center()
                            .justify_between()
                            .rounded(radius_lg)
                            .border_1()
                            .border_color(border)
                            .bg(card_bg)
                            .child(
                                v_flex()
                                    .flex_1()
                                    .min_w_0()
                                    .gap_0p5()
                                    .child(
                                        div()
                                            .text_sm()
                                            .font_weight(gpui::FontWeight::MEDIUM)
                                            .child("Diagnostics"),
                                    )
                                    .child(
                                        div().text_xs().text_color(muted_fg).child(
                                            "Copies versions, theme/motion mode, and safe mock status with no paths, titles, or secrets.",
                                        ),
                                    ),
                            )
                            .child(
                                Button::new("about-copy-diagnostics")
                                    .primary()
                                    .label("Copy diagnostics")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.copy_diagnostics(cx);
                                    })),
                            ),
                    )
                    .children(self.status_message.clone().map(|(is_err, msg)| {
                        div()
                            .text_xs()
                            .text_color(if is_err { danger_fg } else { muted_fg })
                            .child(msg)
                    })),
            )
    }
}
