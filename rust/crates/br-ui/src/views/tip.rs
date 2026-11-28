// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

use gpui::{
    App, ClickEvent, IntoElement, ParentElement as _, Styled as _, Window, div,
};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::{ActiveTheme as _, h_flex, v_flex};

/// Reusable first-visit tip card (TeachingTip equivalent), drawn at the top of a page's content.
pub fn render_tip(
    dismiss_id: &'static str,
    title: &'static str,
    message: &'static str,
    on_dismiss: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    cx: &App,
) -> impl IntoElement {
    let theme = cx.theme();
    h_flex()
        .w_full()
        .p_4()
        .gap_4()
        .items_center()
        .justify_between()
        .rounded(theme.radius_lg)
        .border_1()
        .border_color(theme.info)
        .bg(theme.group_box)
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .gap_1()
                .child(
                    div()
                        .text_sm()
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .child(title),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(message),
                ),
        )
        .child(
            Button::new(dismiss_id)
                .secondary()
                .label("Got it")
                .on_click(on_dismiss),
        )
}
