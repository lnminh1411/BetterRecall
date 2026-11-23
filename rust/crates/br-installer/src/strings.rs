// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Centralized English UI strings and catalog-backed model size labels for the installer wizard.

use std::path::Path;
use std::time::Duration;

use br_fetch::{Catalog, ModelSet};

use crate::install::ModelMode;
use crate::progress::Step;

pub const WINDOW_TITLE: &str = "BetterRecall Setup";

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
pub const READY_MODELS_LABEL: &str = "Models folder:";
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

pub const FINISH_MODELS_OFFLINE: &str = "Models were not downloaded because there is no internet connection. Add them later from the Models page or by running Setup again.";
pub const FINISH_MODELS_CANCELLED: &str = "Model download was cancelled. BetterRecall is installed, and your model selection was saved. Run Setup again (Apps > Installed apps > Modify) or open the Models page in the app to finish downloading.";

pub const HINT_IN_USE: &str = "Quit BetterRecall from the tray and run Setup again.";
pub const HINT_ALREADY_RUNNING: &str =
    "Another BetterRecall Setup window is already open. Close it and try again.";
pub const HINT_PAYLOAD: &str =
    "The installer file may be incomplete or damaged. Download BetterRecall Setup again.";
pub const HINT_MODELS: &str = "Run Setup again (Apps > Installed apps > Modify) or open the Models page in the app to try again.";
pub const HINT_INTEGRITY: &str =
    "BetterRecall's files were changed or are damaged. Download BetterRecall Setup again and reinstall.";

/// Formats a byte count in decimal GB (e.g. `about 6.6 GB`, `about 3.5 GB`).
pub fn format_approx_gb(bytes: u64) -> String {
    format!("about {:.1} GB", bytes as f64 / 1_000_000_000.0)
}

/// Radio button label for `mode` on the Models wizard page, using sizes from `catalog`.
pub fn model_mode_radio_label(mode: ModelMode, catalog: Option<&Catalog>) -> String {
    match mode {
        ModelMode::Full => match catalog {
            Some(c) => format!("Full ({})", format_approx_gb(c.bytes_for(ModelSet::Full))),
            None => "Full".to_string(),
        },
        ModelMode::Minimal => match catalog {
            Some(c) => format!(
                "Minimal ({})",
                format_approx_gb(c.bytes_for(ModelSet::Minimal))
            ),
            None => "Minimal".to_string(),
        },
        ModelMode::Later => "Install models later".to_string(),
    }
}

/// Description text shown beneath each radio button on the Models wizard page.
pub fn model_mode_description(mode: ModelMode) -> &'static str {
    match mode {
        ModelMode::Full => {
            "Search, OCR, privacy filters, audio transcription and the SigLIP 2 fallback model."
        }
        ModelMode::Minimal => {
            "Search, OCR and privacy filters; no audio transcription or SigLIP 2 fallback."
        }
        ModelMode::Later => {
            "Nothing is downloaded now; add models later from the app's Models page or by running Setup again from Apps > Installed apps."
        }
    }
}

/// Summary line on the Ready page showing the selected model set and approximate download size.
pub fn ready_models_summary(mode: ModelMode, catalog: Option<&Catalog>) -> String {
    match mode {
        ModelMode::Full => match catalog {
            Some(c) => format!(
                "Models to download: Full ({})",
                format_approx_gb(c.bytes_for(ModelSet::Full))
            ),
            None => "Models to download: Full".to_string(),
        },
        ModelMode::Minimal => match catalog {
            Some(c) => format!(
                "Models to download: Minimal ({})",
                format_approx_gb(c.bytes_for(ModelSet::Minimal))
            ),
            None => "Models to download: Minimal".to_string(),
        },
        ModelMode::Later => "Models to download: None (install later)".to_string(),
    }
}

/// Formats live model download progress with transferred bytes and transfer rate in MB/s.
pub fn format_download_progress(done: u64, total: u64, elapsed: Duration) -> String {
    let base = format!("{} / {}", format_size(done), format_size(total));
    let secs = elapsed.as_secs_f64();
    if done > 0 && secs >= 0.2 {
        let mb_per_sec = (done as f64 / 1_000_000.0) / secs;
        format!("{base} ({mb_per_sec:.1} MB/s)")
    } else {
        base
    }
}

fn format_size(bytes: u64) -> String {
    if bytes >= 1_000_000_000 {
        format!("{:.2} GB", bytes as f64 / 1_000_000_000.0)
    } else {
        format!("{:.1} MB", bytes as f64 / 1_000_000.0)
    }
}

pub fn uninstall_keep_label(home: &Path) -> String {
    format!(
        "Keep my data (screen history, settings and keys stay in {})",
        home.display()
    )
}

pub fn finish_installed(version: &str) -> String {
    format!("BetterRecall {version} is installed.")
}

/// N10 (sweep 6 retest): the download error names the full model URL (too long to wrap on the
/// Finish page) and repeats its cause through the error chain. Show the URL's host only and
/// each part of the chain once.
pub fn short_failure_reason(reason: &str) -> String {
    let words: Vec<String> = reason
        .split(' ')
        .map(|w| {
            let rest = w
                .strip_prefix("https://")
                .or_else(|| w.strip_prefix("http://"));
            match rest {
                Some(r) => {
                    let host = r.split('/').next().unwrap_or(r);
                    let tail = if w.ends_with(':') && !host.ends_with(':') { ":" } else { "" };
                    format!("{host}{tail}")
                }
                None => w.to_string(),
            }
        })
        .collect();
    let joined = words.join(" ");
    let mut seen: Vec<&str> = Vec::new();
    for part in joined.split(": ") {
        if !seen.contains(&part) {
            seen.push(part);
        }
    }
    seen.join(": ")
}

pub fn finish_models_failed(reason: &str) -> String {
    let reason = short_failure_reason(reason);
    format!(
        "Model download did not finish ({reason}). BetterRecall is installed; run Setup again (Apps > Installed apps > Modify) or open the Models page in the app to try again."
    )
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
        Step::DownloadingModels => "Downloading models...",
        Step::SavingSettings => "Saving model selection...",
        Step::Unregistering => "Removing registration...",
        Step::RemovingFiles => "Removing installed files...",
        Step::DeletingData => "Deleting BetterRecall data...",
        Step::Done => "Finishing up...",
    }
}
