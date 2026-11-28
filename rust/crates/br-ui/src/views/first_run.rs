// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

use std::path::PathBuf;

use gpui::{
    AppContext as _, Context, Entity, EventEmitter, FocusHandle, InteractiveElement as _,
    IntoElement, ObjectFit, ParentElement as _, Render, SharedString,
    StatefulInteractiveElement as _, Styled as _, StyledImage as _, Subscription, Window, div, img,
    px,
};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::input::{Input, InputState};
use gpui_component::searchable_list::SearchableListChange;
use gpui_component::select::{Select, SelectDelegate, SelectEvent, SelectItem, SelectState};
use gpui_component::switch::Switch;
use gpui_component::{ActiveTheme as _, Disableable as _, IndexPath, h_flex, v_flex};

pub const TOTAL_STEPS: usize = 6;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnlockMethod {
    WindowsHello,
    Password,
}

impl UnlockMethod {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::WindowsHello => "windows_hello",
            Self::Password => "password",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FirstRunChoices {
    pub capture: bool,
    pub replay: bool,
    pub speech: bool,
    pub autostart: bool,
    pub unlock: UnlockMethod,
}

impl Default for FirstRunChoices {
    fn default() -> Self {
        Self {
            capture: true,
            replay: true,
            speech: false,
            autostart: true,
            unlock: UnlockMethod::WindowsHello,
        }
    }
}

/// Pure state machine for the 6-step first-start introduction flow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FirstRunState {
    pub step: usize,
    pub choices: FirstRunChoices,
    pub completed: bool,
    pub skipped: bool,
}

impl Default for FirstRunState {
    fn default() -> Self {
        Self::new()
    }
}

impl FirstRunState {
    pub fn new() -> Self {
        Self {
            step: 1,
            choices: FirstRunChoices::default(),
            completed: false,
            skipped: false,
        }
    }

    pub fn next(&mut self) {
        if self.step < TOTAL_STEPS {
            self.step += 1;
        }
    }

    pub fn back(&mut self) {
        if self.step > 1 {
            self.step -= 1;
        }
    }

    pub fn skip(&mut self) {
        self.skipped = true;
        self.completed = true;
    }

    pub fn finish(&mut self) {
        self.skipped = false;
        self.completed = true;
    }

    pub fn can_go_back(&self) -> bool {
        self.step > 1
    }

    pub fn is_last_step(&self) -> bool {
        self.step == TOTAL_STEPS
    }

    pub fn to_done_json(&self) -> String {
        format!(
            "{{\"capture\":{},\"replay\":{},\"speech\":{},\"autostart\":{},\"unlock\":\"{}\",\"skipped\":{},\"step\":{}}}",
            self.choices.capture,
            self.choices.replay,
            self.choices.speech,
            self.choices.autostart,
            self.choices.unlock.as_str(),
            self.skipped,
            self.step,
        )
    }
}

#[derive(Clone, PartialEq, Eq)]
struct UnlockOption {
    method: UnlockMethod,
    label: SharedString,
    disabled: bool,
}

impl SelectItem for UnlockOption {
    type Value = UnlockMethod;

    fn title(&self) -> SharedString {
        self.label.clone()
    }

    fn value(&self) -> &Self::Value {
        &self.method
    }

    fn disabled(&self) -> bool {
        self.disabled
    }
}

struct UnlockSelectDelegate {
    items: Vec<UnlockOption>,
}

impl SelectDelegate for UnlockSelectDelegate {
    type Item = UnlockOption;

    fn items_count(&self, _section: usize) -> usize {
        self.items.len()
    }

    fn item(&self, ix: IndexPath) -> Option<&Self::Item> {
        self.items.get(ix.row)
    }

    fn position<V>(&self, value: &V) -> Option<IndexPath>
    where
        Self::Item: SelectItem<Value = V>,
        V: PartialEq,
    {
        self.items
            .iter()
            .position(|item| item.value() == value)
            .map(IndexPath::new)
    }

    fn on_will_change(
        &mut self,
        selection: &mut Vec<(IndexPath, Self::Item)>,
        changes: &[SearchableListChange],
    ) {
        // Veto selecting any disabled option (such as Password before one is configured).
        let selects_disabled = changes.iter().any(|change| match change {
            SearchableListChange::Select { index } => {
                self.item(*index).is_some_and(SelectItem::disabled)
            }
            SearchableListChange::Deselect { .. } => false,
        });
        if selects_disabled {
            return;
        }
        for change in changes {
            match change {
                SearchableListChange::Select { index } => {
                    if let Some(item) = self.item(*index)
                        && !selection.iter().any(|(_, cur)| cur.value() == item.value())
                    {
                        selection.push((*index, item.clone()));
                    }
                }
                SearchableListChange::Deselect { index } => {
                    selection.retain(|(cur_ix, _)| cur_ix != index);
                }
            }
        }
    }
}

pub enum FirstRunEvent {
    Done,
}

pub struct FirstRunView {
    state: FirstRunState,
    assets_dir: Option<PathBuf>,
    demo_search_input: Entity<InputState>,
    unlock_select: Entity<SelectState<UnlockSelectDelegate>>,
    focus_handle: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<FirstRunEvent> for FirstRunView {}

impl FirstRunView {
    pub fn new(
        assets_dir: Option<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let demo_search_input = cx.new(|cx| {
            let mut st = InputState::new(window, cx).placeholder("Search your recall history...");
            st.set_value("invoice", window, cx);
            st
        });

        let unlock_select = cx.new(|cx| {
            SelectState::new(
                UnlockSelectDelegate {
                    items: vec![
                        UnlockOption {
                            method: UnlockMethod::WindowsHello,
                            label: "Windows Hello".into(),
                            disabled: false,
                        },
                        UnlockOption {
                            method: UnlockMethod::Password,
                            label: "Password".into(),
                            disabled: true,
                        },
                    ],
                },
                Some(IndexPath::new(0)),
                window,
                cx,
            )
        });

        let mut subscriptions = Vec::new();
        subscriptions.push(cx.subscribe(
            &unlock_select,
            |this, _, ev: &SelectEvent<UnlockSelectDelegate>, cx| {
                if let SelectEvent::Confirm(Some(method)) = ev
                    && *method != UnlockMethod::Password
                {
                    this.state.choices.unlock = *method;
                    cx.notify();
                }
            },
        ));

        let focus_handle = cx.focus_handle();

        Self {
            state: FirstRunState::new(),
            assets_dir,
            demo_search_input,
            unlock_select,
            focus_handle,
            _subscriptions: subscriptions,
        }
    }

    pub fn focus_handle(&self) -> FocusHandle {
        self.focus_handle.clone()
    }

    pub fn reset(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.state = FirstRunState::new();
        self.unlock_select.update(cx, |sel, cx| {
            sel.set_selected_index(Some(IndexPath::new(0)), window, cx);
        });
        cx.notify();
    }

    fn finish_flow(&mut self, skipped: bool, cx: &mut Context<Self>) {
        if skipped {
            self.state.skip();
        } else {
            self.state.finish();
        }
        eprintln!("firstrun-done {}", self.state.to_done_json());
        cx.emit(FirstRunEvent::Done);
        cx.notify();
    }

    fn render_step(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = cx.theme();
        let card_bg = theme.group_box;
        let border = theme.border;
        let muted_fg = theme.muted_foreground;
        let radius_lg = theme.radius_lg;

        let card = || {
            v_flex()
                .w_full()
                .p_4()
                .gap_2()
                .rounded(radius_lg)
                .border_1()
                .border_color(border)
                .bg(card_bg)
        };

        match self.state.step {
            1 => v_flex()
                .gap_4()
                .child(
                    div()
                        .text_xl()
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .child("Welcome to BetterRecall"),
                )
                .child(
                    card()
                        .child(
                            div()
                                .text_sm()
                                .font_weight(gpui::FontWeight::MEDIUM)
                                .child("Search your own screen and speech history"),
                        )
                        .child(div().text_sm().text_color(muted_fg).child(
                            "BetterRecall indexes your screen and speech locally so you can find anything you saw or heard in seconds.",
                        ))
                        .child(div().text_sm().text_color(muted_fg).child(
                            "Everything stays on this PC. Nothing is uploaded, and no account is required.",
                        )),
                )
                .into_any_element(),

            2 => v_flex()
                .gap_4()
                .child(
                    div()
                        .text_xl()
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .child("What is captured and what is skipped"),
                )
                .child(
                    card()
                        .child(
                            div()
                                .text_sm()
                                .font_weight(gpui::FontWeight::SEMIBOLD)
                                .child("Captured locally"),
                        )
                        .child(div().text_sm().text_color(muted_fg).child("• Screen keyframes"))
                        .child(
                            div()
                                .text_sm()
                                .text_color(muted_fg)
                                .child("• Instant replay buffer"),
                        )
                        .child(
                            div()
                                .text_sm()
                                .text_color(muted_fg)
                                .child("• Speech, only if you turn it on"),
                        ),
                )
                .child(
                    card()
                        .child(
                            div()
                                .text_sm()
                                .font_weight(gpui::FontWeight::SEMIBOLD)
                                .child("Skipped automatically"),
                        )
                        .child(
                            div()
                                .text_sm()
                                .text_color(muted_fg)
                                .child("• Private / incognito browser windows"),
                        )
                        .child(
                            div()
                                .text_sm()
                                .text_color(muted_fg)
                                .child("• Apps you exclude are never captured (including password managers and remote desktop apps by default)"),
                        )
                        .child(
                            div()
                                .text_sm()
                                .text_color(muted_fg)
                                .child("• Excluded folders and window titles"),
                        )
                        .child(
                            div()
                                .text_sm()
                                .text_color(muted_fg)
                                .child("• BetterRecall itself"),
                        ),
                )
                .child(
                    card()
                        .child(
                            div()
                                .text_sm()
                                .font_weight(gpui::FontWeight::SEMIBOLD)
                                .child("Blacked out automatically"),
                        )
                        .child(
                            div()
                                .text_sm()
                                .text_color(muted_fg)
                                .child("• Password fields and detected sensitive data are blacked out before storage"),
                        ),
                )
                .into_any_element(),

            3 => v_flex()
                .gap_4()
                .child(
                    div()
                        .text_xl()
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .child("Privacy controls"),
                )
                .child(
                    card()
                        .child(
                            div()
                                .text_sm()
                                .font_weight(gpui::FontWeight::MEDIUM)
                                .child("Pause capture anytime"),
                        )
                        .child(div().text_xs().text_color(muted_fg).child(
                            "Press Ctrl+Alt+P or use the tray icon to pause and resume immediately.",
                        )),
                )
                .child(
                    card()
                        .child(
                            div()
                                .text_sm()
                                .font_weight(gpui::FontWeight::MEDIUM)
                                .child("Exclusions"),
                        )
                        .child(div().text_xs().text_color(muted_fg).child(
                            "Apps you exclude are never captured, along with excluded folders and window titles.",
                        )),
                )
                .child(
                    card()
                        .child(
                            div()
                                .text_sm()
                                .font_weight(gpui::FontWeight::MEDIUM)
                                .child("Lock and delete history"),
                        )
                        .child(div().text_xs().text_color(muted_fg).child(
                            "Lock your vault with Windows Hello or a password, and delete any time range or all history at any time.",
                        )),
                )
                .into_any_element(),

            4 => {
                let hotkey_row = |keys: &'static str, desc: &'static str| {
                    h_flex()
                        .w_full()
                        .justify_between()
                        .items_center()
                        .py_1()
                        .child(
                            div()
                                .text_sm()
                                .font_weight(gpui::FontWeight::MEDIUM)
                                .child(keys),
                        )
                        .child(div().text_sm().text_color(muted_fg).child(desc))
                };
                v_flex()
                    .gap_4()
                    .child(
                        div()
                            .text_xl()
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .child("Keyboard shortcuts"),
                    )
                    .child(
                        card()
                            .child(hotkey_row("Ctrl+Alt+Space", "Open search launcher"))
                            .child(hotkey_row("Ctrl+Alt+P", "Pause or resume capture"))
                            .child(hotkey_row("Ctrl+Alt+R", "Save instant replay"))
                            .child(hotkey_row(
                                "Ctrl+Alt+Shift+X",
                                "Panic: pauses capture, clears memory, and locks immediately",
                            )),
                    )
                    .into_any_element()
            }

            5 => {
                let demo_img_path = self.assets_dir.as_ref().map(|d| d.join("mail-invoice.png"));
                v_flex()
                    .gap_4()
                    .child(
                        div()
                            .text_xl()
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .child("Search demo"),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(muted_fg)
                            .child("Type a word you remember — BetterRecall finds matching frames and transcripts."),
                    )
                    .child(
                        Input::new(&self.demo_search_input)
                            .aria_label("Demo search query"),
                    )
                    .child(
                        v_flex()
                            .w(px(360.0))
                            .rounded(radius_lg)
                            .border_1()
                            .border_color(border)
                            .bg(card_bg)
                            .overflow_hidden()
                            .child(match demo_img_path {
                                Some(p) if p.is_file() => img(p)
                                    .w_full()
                                    .h(px(180.0))
                                    .object_fit(ObjectFit::Cover)
                                    .into_any_element(),
                                _ => div()
                                    .w_full()
                                    .h(px(180.0))
                                    .bg(theme.muted)
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .text_xs()
                                    .text_color(muted_fg)
                                    .child("mail-invoice.png (demo preview)")
                                    .into_any_element(),
                            })
                            .child(
                                v_flex()
                                    .p_3()
                                    .gap_0p5()
                                    .child(
                                        div()
                                            .text_sm()
                                            .font_weight(gpui::FontWeight::MEDIUM)
                                            .child("Invoice #4821 — Acme Cloud"),
                                    )
                                    .child(div().text_xs().text_color(muted_fg).child("Outlook")),
                            ),
                    )
                    .into_any_element()
            }

            _ => {
                type ToggleHandler = Box<dyn Fn(&bool, &mut Window, &mut gpui::App) + 'static>;
                let toggle_card = |id: &'static str,
                                   title: &'static str,
                                   subtitle: &'static str,
                                   checked: bool,
                                   on_change: ToggleHandler| {
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
                            Switch::new(id)
                                .accessibility_label(title)
                                .checked(checked)
                                .on_change(move |val, window, cx| on_change(val, window, cx)),
                        )
                };

                v_flex()
                    .gap_3()
                    .child(
                        div()
                            .text_xl()
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .child("Setup"),
                    )
                    .child(toggle_card(
                        "firstrun-capture",
                        "Capture",
                        "Record screen keyframes in the background",
                        self.state.choices.capture,
                        Box::new(cx.listener(|this, checked: &bool, _, cx| {
                            this.state.choices.capture = *checked;
                            cx.notify();
                        })),
                    ))
                    .child(toggle_card(
                        "firstrun-replay",
                        "Instant replay",
                        "Keep a rolling local replay buffer",
                        self.state.choices.replay,
                        Box::new(cx.listener(|this, checked: &bool, _, cx| {
                            this.state.choices.replay = *checked;
                            cx.notify();
                        })),
                    ))
                    .child(toggle_card(
                        "firstrun-speech",
                        "Speech",
                        "Transcribe spoken audio locally",
                        self.state.choices.speech,
                        Box::new(cx.listener(|this, checked: &bool, _, cx| {
                            this.state.choices.speech = *checked;
                            cx.notify();
                        })),
                    ))
                    .child(toggle_card(
                        "firstrun-autostart",
                        "Start with Windows",
                        "Launch BetterRecall automatically when you sign in",
                        self.state.choices.autostart,
                        Box::new(cx.listener(|this, checked: &bool, _, cx| {
                            this.state.choices.autostart = *checked;
                            cx.notify();
                        })),
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
                                            .child("Unlock method"),
                                    )
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(muted_fg)
                                            .child("Choose how to unlock your local vault"),
                                    )
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(muted_fg)
                                            .child("Set a password in Settings > Security first"),
                                    ),
                            )
                            .child(
                                div().w(px(200.0)).child(
                                    Select::new(&self.unlock_select)
                                        .accessibility_label("Unlock method"),
                                ),
                            ),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(muted_fg)
                            .child("Cloud AI stays off. You can turn it on later in Settings."),
                    )
                    .into_any_element()
            }
        }
    }
}

impl Render for FirstRunView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let can_back = self.state.can_go_back();
        let is_last = self.state.is_last_step();
        let step_num = self.state.step;

        let step_body = self.render_step(cx);
        let theme = cx.theme();
        let muted_fg = theme.muted_foreground;

        div()
            .id("firstrun-scroll")
            .track_focus(&self.focus_handle)
            .size_full()
            .overflow_y_scroll()
            .p_6()
            .child(
                v_flex()
                    .max_w(px(760.0))
                    .gap_6()
                    .child(
                        div()
                            .text_xs()
                            .text_color(muted_fg)
                            .child(format!("Step {step_num} of {TOTAL_STEPS}")),
                    )
                    .child(step_body)
                    .child(
                        h_flex()
                            .w_full()
                            .pt_4()
                            .border_t_1()
                            .border_color(theme.border)
                            .items_center()
                            .justify_between()
                            .child(
                                Button::new("firstrun-skip")
                                    .ghost()
                                    .label("Skip introduction")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.finish_flow(true, cx);
                                    })),
                            )
                            .child(
                                h_flex()
                                    .gap_2()
                                    .child(
                                        Button::new("firstrun-back")
                                            .secondary()
                                            .disabled(!can_back)
                                            .label("Back")
                                            .on_click(cx.listener(|this, _, window, cx| {
                                                this.state.back();
                                                if !this.state.can_go_back() {
                                                    window.focus(&this.focus_handle, cx);
                                                }
                                                cx.notify();
                                            })),
                                    )
                                    .child(
                                        // Share one button id slot so keyboard focus stays on the
                                        // primary action when Next turns into Finish on the last step.
                                        Button::new("firstrun-primary")
                                            .primary()
                                            .label(if is_last { "Finish" } else { "Next" })
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                if is_last {
                                                    this.finish_flow(false, cx);
                                                } else {
                                                    this.state.next();
                                                    cx.notify();
                                                }
                                            })),
                                    ),
                            ),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn step_bounds_next_and_back() {
        let mut s = FirstRunState::new();
        assert_eq!(s.step, 1);
        assert!(!s.can_go_back());
        s.back();
        assert_eq!(s.step, 1);

        for expected in 2..=TOTAL_STEPS {
            s.next();
            assert_eq!(s.step, expected);
        }
        assert!(s.is_last_step());
        assert!(s.can_go_back());
        s.next();
        assert_eq!(s.step, TOTAL_STEPS);

        s.back();
        assert_eq!(s.step, TOTAL_STEPS - 1);
    }

    #[test]
    fn skip_marks_completed_and_skipped() {
        let mut s = FirstRunState::new();
        s.next();
        s.skip();
        assert!(s.completed);
        assert!(s.skipped);
        let json = s.to_done_json();
        assert!(json.contains("\"skipped\":true"));
        assert!(json.contains("\"step\":2"));
    }

    #[test]
    fn finish_marks_completed_and_not_skipped() {
        let mut s = FirstRunState::new();
        for _ in 1..TOTAL_STEPS {
            s.next();
        }
        s.choices.capture = false;
        s.choices.unlock = UnlockMethod::Password;
        s.finish();
        assert!(s.completed);
        assert!(!s.skipped);
        let json = s.to_done_json();
        assert!(json.contains("\"capture\":false"));
        assert!(json.contains("\"speech\":false"));
        assert!(json.contains("\"unlock\":\"password\""));
        assert!(json.contains("\"skipped\":false"));
        assert!(json.contains("\"step\":6"));
    }

    #[test]
    fn default_choices_speech_is_off() {
        let s = FirstRunState::new();
        assert!(s.choices.capture);
        assert!(s.choices.replay);
        assert!(!s.choices.speech);
        assert!(s.choices.autostart);
        assert!(s.to_done_json().contains("\"speech\":false"));
    }
}
