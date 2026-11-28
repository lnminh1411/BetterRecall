// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

#[cfg(windows)]
pub mod audio;
#[cfg(windows)]
pub mod clipboard;
#[cfg(windows)]
pub mod events;
#[cfg(windows)]
pub mod shell;
#[cfg(windows)]
pub mod video;
#[cfg(windows)]
pub mod window;

#[cfg(windows)]
pub use window::{
    apply_display_affinity, client_area_animation_enabled, configure_launcher_hwnd,
    display_for_cursor, hwnd_from_window, set_launcher_topmost, working_set_mb,
};
