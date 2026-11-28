// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

use gpui::{
    Context, EventEmitter, InteractiveElement as _, IntoElement, ParentElement as _, Render,
    StatefulInteractiveElement as _, Styled as _, Window, div, px,
};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::{ActiveTheme as _, h_flex, v_flex};

use crate::links;

pub enum HelpEvent {
    ShowIntroduction,
}

pub struct HelpView {
    no_shell: bool,
    status_message: Option<(bool, String)>,
}

impl EventEmitter<HelpEvent> for HelpView {}

impl HelpView {
    pub fn new(no_shell: bool) -> Self {
        Self {
            no_shell,
            status_message: None,
        }
    }

    fn open_user_guide(&mut self, cx: &mut Context<Self>) {
        match links::open_url(links::USER_GUIDE, self.no_shell) {
            Ok(()) => {
                self.status_message = Some((
                    false,
                    if self.no_shell {
                        format!("Link logged (--mock-no-shell): {}", links::USER_GUIDE)
                    } else {
                        "Opened user guide in your default browser.".to_string()
                    },
                ));
            }
            Err(e) => {
                self.status_message = Some((true, format!("Could not open link: {e}")));
            }
        }
        cx.notify();
    }
}

impl Render for HelpView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let card_bg = theme.group_box;
        let border = theme.border;
        let muted_fg = theme.muted_foreground;
        let danger_fg = theme.danger;
        let radius_lg = theme.radius_lg;

        div()
            .id("help-scroll")
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
                            .child("Help"),
                    )
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
                                            .child("Introduction"),
                                    )
                                    .child(
                                        div().text_xs().text_color(muted_fg).child(
                                            "Walk through capture, privacy controls, hotkeys, and initial setup again.",
                                        ),
                                    ),
                            )
                            .child(
                                Button::new("help-show-intro")
                                    .primary()
                                    .label("Show the introduction again")
                                    .on_click(cx.listener(|_this, _, _, cx| {
                                        cx.emit(HelpEvent::ShowIntroduction);
                                    })),
                            ),
                    )
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
                                            .child("User guide"),
                                    )
                                    .child(
                                        div().text_xs().text_color(muted_fg).child(
                                            "Read the BetterRecall user guide on GitHub.",
                                        ),
                                    ),
                            )
                            .child(
                                Button::new("help-user-guide")
                                    .secondary()
                                    .label("User guide")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.open_user_guide(cx);
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
