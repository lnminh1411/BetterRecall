// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

use gpui::{App, Global};

use crate::args::MotionArg;
use crate::platform;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MotionMode {
    System,
    Full,
    Limited,
}

impl From<MotionArg> for MotionMode {
    fn from(arg: MotionArg) -> Self {
        match arg {
            MotionArg::System => Self::System,
            MotionArg::Full => Self::Full,
            MotionArg::Limited => Self::Limited,
        }
    }
}

impl MotionMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::System => "System",
            Self::Full => "Full",
            Self::Limited => "Limited",
        }
    }

    pub fn from_label(label: &str) -> Self {
        match label {
            "Full" => Self::Full,
            "Limited" => Self::Limited,
            _ => Self::System,
        }
    }
}

#[derive(Debug, Clone)]
pub struct MotionState {
    pub mode: MotionMode,
    pub reduced: bool,
}

impl Global for MotionState {}

pub fn init(mode: MotionMode, cx: &mut App) {
    let reduced = compute_reduced(mode);
    cx.set_global(MotionState { mode, reduced });
    cx.set_reduce_motion(reduced);
}

pub fn set_mode(mode: MotionMode, cx: &mut App) {
    let reduced = compute_reduced(mode);
    if cx.has_global::<MotionState>() {
        let state = cx.global_mut::<MotionState>();
        state.mode = mode;
        state.reduced = reduced;
    } else {
        cx.set_global(MotionState { mode, reduced });
    }
    cx.set_reduce_motion(reduced);
    cx.refresh_windows();
}

pub fn refresh_from_system(cx: &mut App) {
    let mode = cx
        .try_global::<MotionState>()
        .map(|s| s.mode)
        .unwrap_or(MotionMode::System);
    if mode == MotionMode::System {
        let reduced = compute_reduced(mode);
        if cx.has_global::<MotionState>() {
            cx.global_mut::<MotionState>().reduced = reduced;
        }
        cx.set_reduce_motion(reduced);
        cx.refresh_windows();
    }
}

pub fn current_mode(cx: &App) -> MotionMode {
    cx.try_global::<MotionState>()
        .map(|s| s.mode)
        .unwrap_or(MotionMode::System)
}

pub fn is_reduced(cx: &App) -> bool {
    cx.reduce_motion()
}

fn compute_reduced(mode: MotionMode) -> bool {
    match mode {
        MotionMode::Full => false,
        MotionMode::Limited => true,
        MotionMode::System => !platform::client_area_animation_enabled(),
    }
}
