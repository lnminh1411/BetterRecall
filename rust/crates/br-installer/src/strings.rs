// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Centralized English UI strings and model size descriptions for the installer wizard.

use std::path::Path;

use crate::install::ModelMode;
use crate::progress::Step;

pub const WINDOW_TITLE: &str = "BetterRecall Setup";

/// Display metadata for one [`ModelMode`] choice on the Models wizard page.
/// I3 will replace the static size text with values from the model manifest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelModeInfo {
    pub mode: ModelMode,
    pub size_label: &'static str,
    pub radio_label: &'static str,
    pub description: &'static str,
}

/// Ordered model mode choices shown on the Models page.
pub const MODEL_MODES: [ModelModeInfo; 3] = [
    ModelModeInfo {
        mode: ModelMode::Full,
        size_label: "about 3.9 GB",
        radio_label: "Full (about 3.9 GB)",
        description: "Search, OCR, privacy filters, audio transcription and the NPU image model.",
    },
    ModelModeInfo {
        mode: ModelMode::Minimal,
        size_label: "about 2 GB",
        radio_label: "Minimal (about 2 GB)",
        description: "Search, OCR and privacy filters; no audio transcription.",
    },
    ModelModeInfo {
        mode: ModelMode::Later,
        size_label: "",
        radio_label: "Install models later",
        description: "Nothing is downloaded now; add models later from the app's Models page or by running Setup again from Apps > Installed apps.",
    },
];

pub const LICENCE_HEADER: &str = "Licence Agreement";
pub const LICENCE_WELCOME: &str =
    "Welcome to BetterRecall Setup. Please review the licence agreement before continuing.";
pub const LICENCE_MISSING: &str = "Licence text not found in this installer.";
pub const LICENCE_THIRD_PARTY: &str =
    r"Third-party licences are listed in LICENSES\third-party\README.txt in the install folder.";
pub const LICENCE_ACCEPT: &str = "I accept the licence";

pub const MODELS_HEADER: &str = "Choose Models";
pub const MODELS_NOTE: &str = "Models are downloaded from Hugging Face after the app is installed.";

pub const READY_HEADER: &str = "Ready to Install";
pub const READY_INSTALL_LABEL: &str = "Install folder:";
pub const READY_DATA_LABEL: &str = "Data folder:";
pub const READY_START_CHECK: &str = "Start BetterRecall when Setup finishes";

pub const UNINSTALL_HEADER: &str = "Uninstall BetterRecall";
pub const UNINSTALL_DELETE: &str = "Delete my data";
pub const UNINSTALL_WARNING: &str = "This permanently deletes your screen history, replays, keys and downloaded models. It cannot be undone.";
pub const UNINSTALL_UNDERSTAND: &str = "I understand";

pub const PROGRESS_HEADER_INSTALL: &str = "Installing BetterRecall";
pub const PROGRESS_HEADER_MODIFY: &str = "Modifying BetterRecall";
pub const PROGRESS_HEADER_UNINSTALL: &str = "Uninstalling BetterRecall";
pub const PROGRESS_CANCELLING: &str = "Cancelling...";

pub const FINISH_HEADER_INSTALL: &str = "Installation Complete";
pub const FINISH_HEADER_MODIFY: &str = "Modify Complete";
pub const FINISH_HEADER_UNINSTALL: &str = "Uninstall Complete";
pub const FINISH_HEADER_CANCELLED: &str = "Setup Cancelled";
pub const FINISH_HEADER_ERROR: &str = "Setup Failed";

pub const BTN_INSTALL: &str = "Install";
pub const BTN_UNINSTALL: &str = "Uninstall";
pub const BTN_NEXT: &str = "Next";

pub const FINISH_MODIFIED: &str = "Model selection has been saved.";
pub const FINISH_UNINSTALLED: &str = "BetterRecall has been uninstalled.";
pub const FINISH_UNINSTALLED_REBOOT: &str =
    "BetterRecall has been uninstalled. A reboot is needed to remove the remaining setup files.";
pub const FINISH_CANCELLED: &str = "Setup was cancelled. Nothing was changed.";
pub const FINISH_NOTHING_TO_DO: &str = "Nothing to modify.";

pub const HINT_IN_USE: &str = "Quit BetterRecall from the tray and run Setup again.";
pub const HINT_ALREADY_RUNNING: &str =
    "Another BetterRecall Setup window is already open. Close it and try again.";
pub const HINT_PAYLOAD: &str =
    "The installer file may be incomplete or damaged. Download BetterRecall Setup again.";

pub fn uninstall_keep_label(home: &Path) -> String {
    format!(
        "Keep my data (screen history, settings and keys stay in {})",
        home.display()
    )
}

pub fn finish_installed(version: &str) -> String {
    format!("BetterRecall {version} is installed.")
}

pub fn hint_not_enough_space(needed: u64, available: u64) -> String {
    const GIB: f64 = 1_073_741_824.0;
    format!(
        "Need {:.2} GB of free disk space, only {:.2} GB available.",
        needed as f64 / GIB,
        available as f64 / GIB
    )
}

pub fn hint_downgrade(installed: &str, incoming: &str) -> String {
    format!(
        "BetterRecall {installed} is already installed, which is newer than this installer ({incoming})."
    )
}

pub fn step_label(step: Step) -> &'static str {
    match step {
        Step::Verifying => "Verifying the installer...",
        Step::Extracting => "Extracting files...",
        Step::Closing => "Closing BetterRecall...",
        Step::Swapping => "Moving files into place...",
        Step::Registering => "Registering BetterRecall...",
        Step::SavingSettings => "Saving model selection...",
        Step::Unregistering => "Removing registration...",
        Step::RemovingFiles => "Removing installed files...",
        Step::DeletingData => "Deleting BetterRecall data...",
        Step::Done => "Finishing up...",
    }
}
