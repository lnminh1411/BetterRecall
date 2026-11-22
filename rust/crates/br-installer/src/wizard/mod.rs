// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Windows Aero Wizard (`PropertySheetW` with `PSH_WIZARD | PSH_AEROWIZARD`).

pub mod ids;
pub mod model;
pub mod ui;

pub use ui::run_wizard;
