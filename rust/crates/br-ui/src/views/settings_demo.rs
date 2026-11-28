// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

use gpui::{
    Context, Entity, FocusHandle, InteractiveElement as _, IntoElement, KeyDownEvent,
    ParentElement as _, Render, SharedString, Styled as _, Subscription, Window, div, px,
};
use gpui_component::accordion::Accordion;
use gpui_component::button::{Button, ButtonVariant, ButtonVariants as _};
use gpui_component::combobox::{Combobox, ComboboxState};
use gpui_component::date_picker::{DatePicker, DatePickerState};
use gpui_component::dialog::DialogButtonProps;
use gpui_component::input::{Input, InputState, NumberInput, Textarea, TextareaState};
use gpui_component::progress::Progress;
use gpui_component::select::{SearchableVec, Select, SelectEvent, SelectState};
use gpui_component::slider::{Slider, SliderEvent, SliderState, SliderValue};
use gpui_component::spinner::Spinner;
use gpui_component::switch::Switch;
use gpui_component::time_field::{HourCycle, TimeField, TimeFieldState};
use gpui_component::{ActiveTheme as _, IndexPath, WindowExt as _, h_flex, v_flex};

use crate::motion::{self, MotionMode};

pub struct SettingsDemoView {
    capture_enabled: bool,
    slider_state: Entity<SliderState>,
    slider_focus: FocusHandle,
    slider_value: f32,
    motion_select: Entity<SelectState<SearchableVec<SharedString>>>,
    model_combobox: Entity<ComboboxState<SearchableVec<SharedString>>>,
    retention_input: Entity<InputState>,
    password_input: Entity<InputState>,
    notes_textarea: Entity<TextareaState>,
    date_picker: Entity<DatePickerState>,
    time_field: Entity<TimeFieldState>,
    accordion_open: bool,
    delete_status: Option<String>,
    _subscriptions: Vec<Subscription>,
}

impl SettingsDemoView {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let slider_state = cx.new(|_| {
            SliderState::new()
                .min(0.0)
                .max(100.0)
                .step(10.0)
                .default_value(60.0)
        });
        let slider_focus = cx.focus_handle().tab_stop(true);

        let initial_motion_ix = match motion::current_mode(cx) {
            MotionMode::System => 0,
            MotionMode::Full => 1,
            MotionMode::Limited => 2,
        };
        let motion_select = cx.new(|cx| {
            SelectState::new(
                SearchableVec::new(vec!["System".into(), "Full".into(), "Limited".into()]),
                Some(IndexPath::new(initial_motion_ix)),
                window,
                cx,
            )
        });

        let model_combobox = cx.new(|cx| {
            ComboboxState::new(
                SearchableVec::new(vec![
                    "Whisper Large v3 Turbo".into(),
                    "Whisper Medium EN".into(),
                    "PaddleOCR v4".into(),
                    "All-MiniLM-L6-v2".into(),
                ]),
                vec![IndexPath::new(0)],
                window,
                cx,
            )
            .searchable(true)
        });

        let retention_input = cx.new(|cx| {
            let mut st = InputState::new(window, cx)
                .min(1.0)
                .max(365.0)
                .step(1.0)
                .placeholder("Days");
            st.set_value("30", window, cx);
            st
        });

        let password_input = cx.new(|cx| {
            InputState::new(window, cx)
                .masked(true)
                .placeholder("Enter vault passphrase")
        });

        let notes_textarea = cx.new(|cx| {
            let mut st = TextareaState::new(window, cx)
                .rows(3)
                .placeholder("One excluded domain or window title per line...");
            st.set_value("bank.example.com\n1Password", window, cx);
            st
        });

        let date_picker = cx.new(|cx| DatePickerState::new(window, cx));
        let time_field =
            cx.new(|cx| TimeFieldState::new(window, cx).hour_cycle(HourCycle::H24));

        let mut subscriptions = Vec::new();
        subscriptions.push(cx.subscribe(
            &slider_state,
            |this, _, ev: &SliderEvent, cx| match ev {
                SliderEvent::Change(val) | SliderEvent::Release(val) => {
                    this.slider_value = val.end();
                    cx.notify();
                }
            },
        ));
        subscriptions.push(cx.subscribe(
            &motion_select,
            |_this, _, ev: &SelectEvent<SearchableVec<SharedString>>, cx| {
                if let SelectEvent::Confirm(Some(label)) = ev {
                    let mode = MotionMode::from_label(label.as_ref());
                    motion::set_mode(mode, cx);
                }
            },
        ));

        Self {
            capture_enabled: true,
            slider_state,
            slider_focus,
            slider_value: 60.0,
            motion_select,
            model_combobox,
            retention_input,
            password_input,
            notes_textarea,
            date_picker,
            time_field,
            accordion_open: false,
            delete_status: None,
            _subscriptions: subscriptions,
        }
    }

    fn step_slider(&mut self, delta: f32, window: &mut Window, cx: &mut Context<Self>) {
        let next = (self.slider_value + delta).clamp(0.0, 100.0);
        self.slider_value = next;
        self.slider_state.update(cx, |s, cx| {
            s.set_value(SliderValue::Single(next), window, cx);
        });
        cx.notify();
    }
}

impl Render for SettingsDemoView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let card_bg = theme.group_box;
        let border = theme.border;
        let muted_fg = theme.muted_foreground;
        let radius_lg = theme.radius_lg;
        let reduced = motion::is_reduced(cx);

        let card = |title: &'static str, subtitle: &'static str, control: gpui::AnyElement| {
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
                .child(control)
        };

        let ticks = h_flex().w_full().justify_between().px_1().children(
            (0..=10).map(|i| {
                div()
                    .w(px(1.0))
                    .h(px(6.0))
                    .bg(muted_fg.opacity(0.6))
                    .id(("tick", i))
            }),
        );

        let slider_control = v_flex()
            .id("slider-keyboard-wrapper")
            .w(px(260.0))
            .gap_1()
            .p_1()
            .rounded(theme.radius)
            .border_1()
            .border_color(theme.transparent)
            .track_focus(&self.slider_focus)
            .focus_visible(|s| s.border_color(theme.ring))
            .on_key_down(cx.listener(|this, ev: &KeyDownEvent, window, cx| {
                match ev.keystroke.key.as_str() {
                    "left" | "down" => this.step_slider(-10.0, window, cx),
                    "right" | "up" => this.step_slider(10.0, window, cx),
                    _ => {}
                }
            }))
            .child(
                h_flex()
                    .justify_between()
                    .child(div().text_xs().text_color(muted_fg).child("Quality"))
                    .child(div().text_xs().child(format!("{}%", self.slider_value as i32))),
            )
            .child(Slider::new(&self.slider_state))
            .child(ticks)
            .into_any_element();

        div()
            .id("settings-scroll")
            .size_full()
            .overflow_y_scroll()
            .p_6()
            .child(
                v_flex()
                    .max_w(px(860.0))
                    .gap_3()
                    .child(
                        div()
                            .text_xl()
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .child("Settings (Keyboard & Controls Demo)"),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(muted_fg)
                            .child("Use Tab / Shift+Tab to reach every control, Arrow keys on Select and Slider, and Escape to close the dialog."),
                    )
                    .child(card(
                        "Screen capture",
                        "Enable background capture while BetterRecall is running",
                        Switch::new("setting-capture-switch")
                            .accessibility_label("Screen capture")
                            .checked(self.capture_enabled)
                            .on_change(cx.listener(|this, checked: &bool, _, cx| {
                                this.capture_enabled = *checked;
                                cx.notify();
                            }))
                            .into_any_element(),
                    ))
                    .child(card(
                        "Capture quality (Slider with ticks)",
                        "Focus with Tab and adjust with Left/Right arrow keys",
                        slider_control,
                    ))
                    .child(card(
                        "Motion (Limited Motion)",
                        "Follow Windows Animation effects, force Full, or force Limited",
                        div()
                            .w(px(220.0))
                            .child(
                                Select::new(&self.motion_select)
                                    .accessibility_label("Motion preference"),
                            )
                            .into_any_element(),
                    ))
                    .child(card(
                        "Model profile (Searchable Combobox)",
                        "Search and select a local inference model",
                        div()
                            .w(px(260.0))
                            .child(
                                Combobox::new(&self.model_combobox)
                                    .placeholder("Choose model..."),
                            )
                            .into_any_element(),
                    ))
                    .child(card(
                        "Retention days (NumberInput)",
                        "Days to keep local captures before pruning",
                        div()
                            .w(px(160.0))
                            .child(NumberInput::new(&self.retention_input))
                            .into_any_element(),
                    ))
                    .child(card(
                        "Vault passphrase (Masked Input)",
                        "Password field with reveal toggle",
                        div()
                            .w(px(260.0))
                            .child(
                                Input::new(&self.password_input)
                                    .aria_label("Vault passphrase")
                                    .mask_toggle(),
                            )
                            .into_any_element(),
                    ))
                    .child(card(
                        "Excluded domains (Multiline Textarea)",
                        "Multiline text entry for privacy exclusions",
                        div()
                            .w(px(320.0))
                            .child(
                                Textarea::new(&self.notes_textarea)
                                    .aria_label("Excluded domains")
                                    .h(px(84.0)),
                            )
                            .into_any_element(),
                    ))
                    .child(card(
                        "Schedule date & time (DatePicker + TimeField)",
                        "Pick a date and 24-hour time window",
                        h_flex()
                            .gap_2()
                            .child(
                                div()
                                    .w(px(170.0))
                                    .child(DatePicker::new(&self.date_picker).placeholder("Pick date")),
                            )
                            .child(TimeField::new(&self.time_field))
                            .into_any_element(),
                    ))
                    .child(
                        Accordion::new("settings-advanced-accordion")
                            .on_toggle_click(cx.listener(|this, open: &[usize], _, cx| {
                                this.accordion_open = open.contains(&0);
                                cx.notify();
                            }))
                            .item(|item| {
                                item.title("Advanced privacy options (Expander)")
                                    .open(self.accordion_open)
                                    .child(
                                        div()
                                            .text_sm()
                                            .text_color(muted_fg)
                                            .child("Private browser windows, UAC prompts, and password managers are excluded automatically."),
                                    )
                            }),
                    )
                    .child(
                        v_flex()
                            .p_4()
                            .gap_3()
                            .rounded(radius_lg)
                            .border_1()
                            .border_color(border)
                            .bg(card_bg)
                            .child(
                                h_flex()
                                    .justify_between()
                                    .items_center()
                                    .child(
                                        div()
                                            .text_sm()
                                            .font_weight(gpui::FontWeight::MEDIUM)
                                            .child("Indexing progress & busy indicator"),
                                    )
                                    .child(
                                        h_flex()
                                            .gap_2()
                                            .items_center()
                                            .child(Spinner::new())
                                            .child(div().text_xs().text_color(muted_fg).child(
                                                if reduced {
                                                    "Static (Limited Motion)"
                                                } else {
                                                    "Spinning"
                                                },
                                            )),
                                    ),
                            )
                            .child(
                                Progress::new("settings-progress-determinate")
                                    .accessibility_label("Indexing progress")
                                    .value(65.0),
                            )
                            .child(
                                Progress::new("settings-progress-indeterminate")
                                    .accessibility_label("Background sync")
                                    .loading(true),
                            ),
                    )
                    .child(card(
                        "Danger zone",
                        "Open a confirmation dialog (Escape closes)",
                        h_flex()
                            .gap_2()
                            .items_center()
                            .children(self.delete_status.clone().map(|s| {
                                div().text_xs().text_color(muted_fg).child(s)
                            }))
                            .child(
                                Button::new("settings-delete-btn")
                                    .danger()
                                    .label("Delete…")
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        let entity = cx.entity().downgrade();
                                        window.open_alert_dialog(cx, move |alert, _, _| {
                                            let entity_ok = entity.clone();
                                            let entity_cancel = entity.clone();
                                            alert
                                                .confirm()
                                                .title("Delete captured history?")
                                                .description("This permanently removes the mock captures in this spike session.")
                                                .button_props(
                                                    DialogButtonProps::default()
                                                        .ok_text("Delete")
                                                        .ok_variant(ButtonVariant::Danger)
                                                        .cancel_text("Cancel")
                                                        .on_ok(move |_, _, cx| {
                                                            let _ = entity_ok.update(cx, |v, cx| {
                                                                v.delete_status = Some("Deleted (mock)".into());
                                                                cx.notify();
                                                            });
                                                            true
                                                        })
                                                        .on_cancel(move |_, _, cx| {
                                                            let _ = entity_cancel.update(cx, |v, cx| {
                                                                v.delete_status = Some("Cancelled".into());
                                                                cx.notify();
                                                            });
                                                            true
                                                        }),
                                                )
                                        });
                                        this.delete_status = None;
                                    })),
                            )
                            .into_any_element(),
                    )),
            )
    }
}
