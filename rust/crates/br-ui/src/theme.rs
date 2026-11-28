// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

use gpui::{App, Global, Hsla, Rgba, SharedString, hsla_to_rgba, px, rgb_to_hsla};
use gpui_component::{Theme, ThemeMode};
use native_theme::color::Rgba as NtRgba;
use native_theme::theme::ColorMode;
use native_theme::{ResolvedTheme, SystemTheme};

#[derive(Debug, Clone)]
pub struct NativeThemeInfo {
    pub is_dark: bool,
    pub high_contrast: bool,
    pub font_family: SharedString,
}

impl Global for NativeThemeInfo {}

fn nt_to_hsla(c: NtRgba) -> Hsla {
    let [r, g, b, a] = c.to_f32_array();
    rgb_to_hsla(Rgba::new(r, g, b, a))
}

fn blend_hsla(a: Hsla, b: Hsla, t: f32) -> Hsla {
    let ar = hsla_to_rgba(a);
    let br = hsla_to_rgba(b);
    let inv = 1.0 - t;
    rgb_to_hsla(Rgba::new(
        ar.color.red * inv + br.color.red * t,
        ar.color.green * inv + br.color.green * t,
        ar.color.blue * inv + br.color.blue * t,
        ar.alpha * inv + br.alpha * t,
    ))
}

fn adjust_lightness(mut c: Hsla, delta: f32) -> Hsla {
    c.color.lightness = (c.color.lightness + delta).clamp(0.0, 1.0);
    c
}

/// Read the active OS theme via `native-theme` and apply it to `gpui_component::Theme`.
pub fn apply_system_theme(cx: &mut App) {
    let Ok(sys) = SystemTheme::from_system() else {
        return;
    };
    let is_dark = sys.mode == ColorMode::Dark;
    let high_contrast = sys.accessibility.high_contrast;
    let resolved = sys.pick(sys.mode);
    apply_resolved(resolved, is_dark, high_contrast, cx);
}

fn apply_resolved(resolved: &ResolvedTheme, is_dark: bool, high_contrast: bool, cx: &mut App) {
    let d = &resolved.defaults;
    let bg = nt_to_hsla(d.background_color);
    let fg = nt_to_hsla(d.text_color);
    let surface = nt_to_hsla(d.surface_color);
    let accent = nt_to_hsla(d.accent_color);
    let accent_fg = nt_to_hsla(d.accent_text_color);
    let muted_fg = nt_to_hsla(d.muted_color);
    let border = nt_to_hsla(d.border.color);
    let danger = nt_to_hsla(d.danger_color);
    let danger_fg = nt_to_hsla(d.danger_text_color);
    let warning = nt_to_hsla(d.warning_color);
    let warning_fg = nt_to_hsla(d.warning_text_color);
    let success = nt_to_hsla(d.success_color);
    let success_fg = nt_to_hsla(d.success_text_color);
    let info = nt_to_hsla(d.info_color);
    let info_fg = nt_to_hsla(d.info_text_color);
    let selection = nt_to_hsla(d.selection_background);
    let link = nt_to_hsla(d.link_color);
    let ring = nt_to_hsla(d.focus_ring_color);

    let hover_delta = if is_dark { 0.06 } else { -0.06 };
    let active_delta = if is_dark { 0.11 } else { -0.11 };
    let primary_hover = adjust_lightness(accent, hover_delta);
    let primary_active = adjust_lightness(accent, active_delta);

    let secondary = if high_contrast {
        bg
    } else {
        blend_hsla(bg, surface, 0.7)
    };
    let secondary_hover = adjust_lightness(secondary, hover_delta);
    let secondary_active = adjust_lightness(secondary, active_delta);
    let muted = if high_contrast {
        bg
    } else {
        blend_hsla(bg, fg, 0.08)
    };
    let card_bg = if high_contrast { bg } else { surface };
    let input_border = if high_contrast {
        fg
    } else {
        blend_hsla(border, fg, 0.15)
    };
    let sidebar_bg = if high_contrast {
        bg
    } else {
        blend_hsla(bg, surface, 0.45)
    };

    let font_family: SharedString = if d.font.family.is_empty() {
        "Segoe UI Variable".into()
    } else {
        d.font.family.to_string().into()
    };
    let mono_family: SharedString = if d.mono_font.family.is_empty() {
        "Cascadia Mono".into()
    } else {
        d.mono_font.family.to_string().into()
    };

    let target_mode = if is_dark {
        ThemeMode::Dark
    } else {
        ThemeMode::Light
    };

    // Switch base mode first (Theme::update replaces colors if mode changes inside it).
    Theme::change(target_mode, None, cx);

    Theme::update(cx, |theme| {
        theme.font_family = font_family.clone();
        theme.mono_font_family = mono_family;
        theme.font_size = px(d.font.size.clamp(12.0, 20.0));
        // WinUI 3 radii: 4px controls, 8px cards/dialogs.
        theme.radius = px(4.0);
        theme.radius_lg = px(8.0);

        let c = &mut theme.colors;
        c.background = bg;
        c.foreground = fg;
        c.border = border;
        c.input = input_border;
        c.ring = ring;
        c.caret = fg;
        c.selection = selection;

        c.primary = accent;
        c.primary_foreground = accent_fg;
        c.primary_hover = primary_hover;
        c.primary_active = primary_active;

        c.button_primary = accent;
        c.button_primary_foreground = accent_fg;
        c.button_primary_hover = primary_hover;
        c.button_primary_active = primary_active;

        c.accent = adjust_lightness(secondary, hover_delta);
        c.accent_foreground = fg;

        c.secondary = secondary;
        c.secondary_foreground = fg;
        c.secondary_hover = secondary_hover;
        c.secondary_active = secondary_active;

        c.button = secondary;
        c.button_foreground = fg;
        c.button_hover = secondary_hover;
        c.button_active = secondary_active;
        c.button_secondary = secondary;
        c.button_secondary_foreground = fg;
        c.button_secondary_hover = secondary_hover;
        c.button_secondary_active = secondary_active;

        c.muted = muted;
        c.muted_foreground = muted_fg;

        c.group_box = card_bg;
        c.group_box_foreground = fg;
        c.popover = card_bg;
        c.popover_foreground = fg;
        c.accordion = card_bg;

        c.sidebar = sidebar_bg;
        c.sidebar_foreground = fg;
        c.sidebar_border = border;
        c.sidebar_accent = adjust_lightness(sidebar_bg, hover_delta);
        c.sidebar_accent_foreground = fg;
        c.sidebar_primary = accent;
        c.sidebar_primary_foreground = accent_fg;

        c.title_bar = bg;
        c.title_bar_border = border;

        c.list = card_bg;
        c.list_hover = secondary_hover;
        c.list_active = blend_hsla(card_bg, accent, 0.18);
        c.list_active_border = accent;

        c.slider_bar = accent;
        c.slider_thumb = accent_fg;
        c.progress_bar = accent;
        c.switch = blend_hsla(bg, fg, 0.25);
        c.switch_thumb = if is_dark { fg } else { bg };

        c.danger = danger;
        c.danger_foreground = danger_fg;
        c.danger_hover = adjust_lightness(danger, hover_delta);
        c.danger_active = adjust_lightness(danger, active_delta);
        c.button_danger = danger;
        c.button_danger_foreground = danger_fg;
        c.button_danger_hover = c.danger_hover;
        c.button_danger_active = c.danger_active;

        c.warning = warning;
        c.warning_foreground = warning_fg;
        c.warning_hover = adjust_lightness(warning, hover_delta);
        c.warning_active = adjust_lightness(warning, active_delta);
        c.button_warning = warning;
        c.button_warning_foreground = warning_fg;
        c.button_warning_hover = c.warning_hover;
        c.button_warning_active = c.warning_active;

        c.success = success;
        c.success_foreground = success_fg;
        c.success_hover = adjust_lightness(success, hover_delta);
        c.success_active = adjust_lightness(success, active_delta);
        c.button_success = success;
        c.button_success_foreground = success_fg;
        c.button_success_hover = c.success_hover;
        c.button_success_active = c.success_active;

        c.info = info;
        c.info_foreground = info_fg;
        c.info_hover = adjust_lightness(info, hover_delta);
        c.info_active = adjust_lightness(info, active_delta);
        c.button_info = info;
        c.button_info_foreground = info_fg;
        c.button_info_hover = c.info_hover;
        c.button_info_active = c.info_active;

        c.link = link;
        c.link_hover = adjust_lightness(link, hover_delta);
        c.link_active = adjust_lightness(link, active_delta);
    });

    cx.set_global(NativeThemeInfo {
        is_dark,
        high_contrast,
        font_family,
    });
}

/// Start the `native-theme` OS watcher and re-apply theme + system motion live on change.
/// Returns the watcher subscription handle (must stay alive for the app lifetime).
pub fn start_theme_watcher(cx: &mut App) -> Option<native_theme::watch::ThemeSubscription> {
    let (tx, rx) = async_channel::bounded::<()>(4);
    let sub = native_theme::watch::on_theme_change(move |_event| {
        let _ = tx.try_send(());
    })
    .ok()?;

    cx.spawn(async move |cx| {
        while rx.recv().await.is_ok() {
            cx.update(|cx| {
                native_theme::detect::invalidate_caches();
                apply_system_theme(cx);
                crate::motion::refresh_from_system(cx);
            });
        }
    })
    .detach();

    Some(sub)
}
