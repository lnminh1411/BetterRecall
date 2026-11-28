// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Pure wizard state machine: page order, button enabling, defaults, radio mapping, and finish text.
//! Tested directly without creating any Win32 window.

use crate::cli::Action;
use crate::error::InstallError;
use crate::install::{ModelMode, ModelsResult, Outcome};
use crate::progress::Step;
use crate::strings;
use crate::wizard::ids::{
    IDC_MODELS_FULL, IDC_MODELS_LATER, IDC_MODELS_MINIMAL, IDD_FINISH, IDD_LICENCE, IDD_MODELS,
    IDD_PROGRESS, IDD_READY, IDD_UNINSTALL_CONFIRM,
};

/// Logical wizard page identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageId {
    Licence,
    Models,
    Ready,
    UninstallConfirm,
    Progress,
    Finish,
}

impl PageId {
    /// Dialog template resource ID corresponding to this page.
    pub fn dialog_id(self) -> u16 {
        match self {
            Self::Licence => IDD_LICENCE,
            Self::Models => IDD_MODELS,
            Self::Ready => IDD_READY,
            Self::UninstallConfirm => IDD_UNINSTALL_CONFIRM,
            Self::Progress => IDD_PROGRESS,
            Self::Finish => IDD_FINISH,
        }
    }

    /// Header title displayed in the Aero Wizard banner for this page.
    pub fn header_title(self, action: Action) -> &'static str {
        match self {
            Self::Licence => strings::LICENCE_HEADER,
            Self::Models => strings::MODELS_HEADER,
            Self::Ready => strings::READY_HEADER,
            Self::UninstallConfirm => strings::UNINSTALL_HEADER,
            Self::Progress => match action {
                Action::Install => strings::PROGRESS_HEADER_INSTALL,
                Action::Modify => strings::PROGRESS_HEADER_MODIFY,
                Action::Uninstall => strings::PROGRESS_HEADER_UNINSTALL,
            },
            Self::Finish => match action {
                Action::Install => strings::FINISH_HEADER_INSTALL,
                Action::Modify => strings::FINISH_HEADER_MODIFY,
                Action::Uninstall => strings::FINISH_HEADER_UNINSTALL,
            },
        }
    }
}

/// Ordered sequence of pages shown for each installer [`Action`].
pub fn pages_for_action(action: Action) -> &'static [PageId] {
    match action {
        Action::Install => &[
            PageId::Licence,
            PageId::Models,
            PageId::Ready,
            PageId::Progress,
            PageId::Finish,
        ],
        Action::Modify => &[PageId::Models, PageId::Progress, PageId::Finish],
        Action::Uninstall => &[PageId::UninstallConfirm, PageId::Progress, PageId::Finish],
    }
}

/// Button state for the active wizard page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WizardButtons {
    pub back: bool,
    pub next: bool,
    pub finish: bool,
    pub cancel: bool,
    /// Custom label for the Next button (`None` uses the standard `"Next"`).
    pub next_label: Option<&'static str>,
    /// Custom label for the Cancel button (`None` uses the standard `"Cancel"`).
    pub cancel_label: Option<&'static str>,
}

/// Inputs used to initialize [`WizardModel`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WizardInit {
    pub action: Action,
    pub existing_model_mode: Option<ModelMode>,
    pub cli_model_mode: Option<ModelMode>,
    pub cli_delete_data: bool,
    pub cli_no_run: bool,
    pub licence_found: bool,
}

impl Default for WizardInit {
    fn default() -> Self {
        Self {
            action: Action::Install,
            existing_model_mode: None,
            cli_model_mode: None,
            cli_delete_data: false,
            cli_no_run: false,
            licence_found: true,
        }
    }
}

/// Pure UI state model for the wizard.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WizardModel {
    pub action: Action,
    pub licence_found: bool,
    pub licence_accepted: bool,
    pub model_mode: ModelMode,
    pub start_after: bool,
    pub delete_data: bool,
    pub delete_understood: bool,
    pub current_step: Option<Step>,
    pub swap_started: bool,
    pub work_done: bool,
}

impl WizardModel {
    /// Creates the initial wizard model with defaults for `init.action`.
    pub fn new(init: WizardInit) -> Self {
        let model_mode = match init.action {
            // C6: default the model choice to stored ModelMode on upgrade (fallback to Minimal)
            Action::Install => init
                .cli_model_mode
                .or(init.existing_model_mode)
                .unwrap_or(ModelMode::Minimal),
            Action::Modify => init
                .cli_model_mode
                .or(init.existing_model_mode)
                .unwrap_or(ModelMode::Later),
            Action::Uninstall => ModelMode::Later,
        };
        Self {
            action: init.action,
            licence_found: init.licence_found,
            licence_accepted: false,
            model_mode,
            start_after: !init.cli_no_run,
            delete_data: init.cli_delete_data,
            delete_understood: false,
            current_step: None,
            swap_started: false,
            work_done: false,
        }
    }

    /// Returns whether Cancel is still permitted during the Progress page.
    /// Cancel is disabled once the file swap starts, and re-enabled during model download
    /// (where it cancels only the download, leaving the app installed).
    pub fn can_cancel(&self) -> bool {
        if self.work_done {
            return false;
        }
        match self.current_step {
            Some(Step::DownloadingModels) => true,
            _ => !self.swap_started,
        }
    }

    /// Updates state when the worker enters `step`.
    pub fn on_step(&mut self, step: Step) {
        self.current_step = Some(step);
        if matches!(
            step,
            Step::Swapping
                | Step::Registering
                | Step::RemovingFiles
                | Step::RemovingModels
                | Step::DeletingData
                | Step::Done
        ) {
            self.swap_started = true;
        }
    }

    /// Computes which wizard buttons should be enabled on `page`.
    pub fn buttons_for(&self, page: PageId) -> WizardButtons {
        match page {
            PageId::Licence => WizardButtons {
                back: false,
                next: self.licence_found && self.licence_accepted,
                finish: false,
                cancel: true,
                next_label: None,
                cancel_label: None,
            },
            PageId::Models => WizardButtons {
                back: self.action == Action::Install,
                next: true,
                finish: false,
                cancel: true,
                next_label: None,
                cancel_label: None,
            },
            PageId::Ready => WizardButtons {
                back: true,
                next: true,
                finish: false,
                cancel: true,
                next_label: Some(strings::BTN_INSTALL),
                cancel_label: None,
            },
            PageId::UninstallConfirm => WizardButtons {
                back: false,
                next: !self.delete_data || self.delete_understood,
                finish: false,
                cancel: true,
                next_label: Some(strings::BTN_UNINSTALL),
                cancel_label: None,
            },
            PageId::Progress => WizardButtons {
                back: false,
                next: false,
                finish: false,
                cancel: self.can_cancel(),
                next_label: None,
                cancel_label: (self.current_step == Some(Step::DownloadingModels) && !self.work_done)
                    .then_some(strings::BTN_STOP_DOWNLOAD),
            },
            PageId::Finish => WizardButtons {
                back: false,
                next: false,
                finish: true,
                cancel: false,
                next_label: None,
                cancel_label: None,
            },
        }
    }

    /// Whether the delete-data warning and "I understand" checkbox are shown on the Uninstall Confirm page.
    pub fn show_delete_warning(&self) -> bool {
        self.delete_data
    }
}

/// Maps a Models page radio button control ID to a [`ModelMode`].
pub fn model_mode_from_radio(control_id: u16) -> Option<ModelMode> {
    match control_id {
        IDC_MODELS_FULL => Some(ModelMode::Full),
        IDC_MODELS_MINIMAL => Some(ModelMode::Minimal),
        IDC_MODELS_LATER => Some(ModelMode::Later),
        _ => None,
    }
}

/// Maps a [`ModelMode`] to its radio button control ID on the Models page.
pub fn radio_from_model_mode(mode: ModelMode) -> u16 {
    match mode {
        ModelMode::Full => IDC_MODELS_FULL,
        ModelMode::Minimal => IDC_MODELS_MINIMAL,
        ModelMode::Later => IDC_MODELS_LATER,
    }
}

/// Scales `(done, total)` byte progress to a `0..=1000` progress bar position.
pub fn scale_progress(done: u64, total: u64) -> u32 {
    if total == 0 {
        return 1000;
    }
    let clamped = done.min(total);
    ((clamped as u128 * 1000) / total as u128) as u32
}

/// Text rendered on the Finish page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FinishMessage {
    pub header: &'static str,
    pub summary: String,
    pub detail: String,
}

/// Formats a successful [`Outcome`] for the Finish page.
pub fn format_finish_outcome(action: Action, outcome: &Outcome) -> FinishMessage {
    let header = PageId::Finish.header_title(action);
    match outcome {
        Outcome::Installed { version, models } => FinishMessage {
            header,
            summary: strings::finish_installed(version),
            detail: format_models_detail(models),
        },
        Outcome::Modified { models, .. } => FinishMessage {
            header,
            summary: strings::FINISH_MODIFIED.to_string(),
            detail: format_models_detail(models),
        },
        Outcome::Uninstalled {
            reboot_required,
            data_folder_kept,
        } => FinishMessage {
            header,
            summary: if *reboot_required {
                strings::FINISH_UNINSTALLED_REBOOT.to_string()
            } else {
                strings::FINISH_UNINSTALLED.to_string()
            },
            // N2: tell the user when delete-data kept the folder due to unknown files
            detail: data_folder_kept
                .as_deref()
                .map(strings::finish_uninstalled_data_kept)
                .unwrap_or_default(),
        },
        Outcome::NothingToDo => FinishMessage {
            header,
            summary: strings::FINISH_NOTHING_TO_DO.to_string(),
            detail: String::new(),
        },
        Outcome::Relaunched => FinishMessage {
            header,
            summary: String::new(),
            detail: String::new(),
        },
    }
}

fn format_models_detail(models: &ModelsResult) -> String {
    match models {
        ModelsResult::Complete | ModelsResult::Skipped => String::new(),
        ModelsResult::Offline => strings::FINISH_MODELS_OFFLINE.to_string(),
        ModelsResult::Cancelled => strings::FINISH_MODELS_CANCELLED.to_string(),
        ModelsResult::Failed(reason) => strings::finish_models_failed(reason),
    }
}

/// Formats an [`InstallError`] with actionable hints for common failures.
pub fn format_finish_error(err: &InstallError) -> FinishMessage {
    match err {
        InstallError::Cancelled => FinishMessage {
            header: strings::FINISH_HEADER_CANCELLED,
            summary: strings::FINISH_CANCELLED.to_string(),
            detail: String::new(),
        },
        InstallError::InUse { .. } => FinishMessage {
            header: strings::FINISH_HEADER_ERROR,
            summary: err.to_string(),
            detail: strings::HINT_IN_USE.to_string(),
        },
        InstallError::NotEnoughSpace { needed, available } => FinishMessage {
            header: strings::FINISH_HEADER_ERROR,
            summary: err.to_string(),
            detail: strings::hint_not_enough_space(*needed, *available),
        },
        InstallError::Downgrade {
            installed,
            incoming,
        } => FinishMessage {
            header: strings::FINISH_HEADER_ERROR,
            summary: err.to_string(),
            detail: strings::hint_downgrade(installed, incoming),
        },
        InstallError::AlreadyRunning => FinishMessage {
            header: strings::FINISH_HEADER_ERROR,
            summary: err.to_string(),
            detail: strings::HINT_ALREADY_RUNNING.to_string(),
        },
        InstallError::Payload(_) => FinishMessage {
            header: strings::FINISH_HEADER_ERROR,
            summary: err.to_string(),
            detail: strings::HINT_PAYLOAD.to_string(),
        },
        InstallError::Models(_) => FinishMessage {
            header: strings::FINISH_HEADER_ERROR,
            summary: err.to_string(),
            detail: strings::HINT_MODELS.to_string(),
        },
        InstallError::Integrity(_) => FinishMessage {
            header: strings::FINISH_HEADER_ERROR,
            summary: err.to_string(),
            detail: strings::HINT_INTEGRITY.to_string(),
        },
        InstallError::Other(_) | InstallError::Usage(_) | InstallError::Io { .. } => {
            FinishMessage {
                header: strings::FINISH_HEADER_ERROR,
                summary: err.to_string(),
                detail: String::new(),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    #[test]
    fn page_order_matches_action() {
        assert_eq!(
            pages_for_action(Action::Install),
            &[
                PageId::Licence,
                PageId::Models,
                PageId::Ready,
                PageId::Progress,
                PageId::Finish
            ]
        );
        assert_eq!(
            pages_for_action(Action::Modify),
            &[PageId::Models, PageId::Progress, PageId::Finish]
        );
        assert_eq!(
            pages_for_action(Action::Uninstall),
            &[PageId::UninstallConfirm, PageId::Progress, PageId::Finish]
        );
    }

    #[test]
    fn default_selections_match_spec() {
        let install = WizardModel::new(WizardInit::default());
        assert_eq!(install.model_mode, ModelMode::Minimal);
        assert!(install.start_after);
        assert!(!install.licence_accepted);

        let install_norun = WizardModel::new(WizardInit {
            cli_no_run: true,
            ..Default::default()
        });
        assert!(!install_norun.start_after);

        let modify = WizardModel::new(WizardInit {
            action: Action::Modify,
            existing_model_mode: Some(ModelMode::Full),
            ..Default::default()
        });
        assert_eq!(modify.model_mode, ModelMode::Full);

        let uninstall = WizardModel::new(WizardInit {
            action: Action::Uninstall,
            ..Default::default()
        });
        assert!(!uninstall.delete_data);
        assert!(!uninstall.delete_understood);
    }

    #[test]
    fn licence_page_button_enabling() {
        let mut m = WizardModel::new(WizardInit {
            licence_found: true,
            ..Default::default()
        });
        assert!(!m.buttons_for(PageId::Licence).next);
        assert!(!m.buttons_for(PageId::Licence).back);
        assert!(m.buttons_for(PageId::Licence).cancel);

        m.licence_accepted = true;
        assert!(m.buttons_for(PageId::Licence).next);

        // Missing licence keeps Next disabled even if accepted is toggled.
        let mut missing = WizardModel::new(WizardInit {
            licence_found: false,
            ..Default::default()
        });
        missing.licence_accepted = true;
        assert!(!missing.buttons_for(PageId::Licence).next);
    }

    #[test]
    fn models_and_ready_buttons() {
        let install = WizardModel::new(WizardInit::default());
        let mb = install.buttons_for(PageId::Models);
        assert!(mb.back);
        assert!(mb.next);

        let rb = install.buttons_for(PageId::Ready);
        assert!(rb.back);
        assert!(rb.next);
        assert_eq!(rb.next_label, Some(strings::BTN_INSTALL));

        let modify = WizardModel::new(WizardInit {
            action: Action::Modify,
            ..Default::default()
        });
        assert!(!modify.buttons_for(PageId::Models).back);
        assert!(modify.buttons_for(PageId::Models).next);
    }

    #[test]
    fn uninstall_confirm_requires_understanding_when_deleting_data() {
        let mut m = WizardModel::new(WizardInit {
            action: Action::Uninstall,
            ..Default::default()
        });
        // Keep data (default): Next ("Uninstall") enabled immediately, warning hidden.
        assert!(!m.show_delete_warning());
        let b = m.buttons_for(PageId::UninstallConfirm);
        assert!(b.next);
        assert_eq!(b.next_label, Some(strings::BTN_UNINSTALL));

        // Delete data without "I understand": Next disabled, warning shown.
        m.delete_data = true;
        assert!(m.show_delete_warning());
        assert!(!m.buttons_for(PageId::UninstallConfirm).next);

        // Delete data with "I understand": Next enabled.
        m.delete_understood = true;
        assert!(m.buttons_for(PageId::UninstallConfirm).next);
    }

    #[test]
    fn progress_cancel_allowed_until_swap_and_finish_only_finish() {
        let mut m = WizardModel::new(WizardInit::default());
        m.on_step(Step::Verifying);
        assert!(m.can_cancel());
        assert!(m.buttons_for(PageId::Progress).cancel);
        assert!(!m.buttons_for(PageId::Progress).back);

        m.on_step(Step::Extracting);
        assert!(m.can_cancel());

        m.on_step(Step::Closing);
        assert!(m.can_cancel());

        m.on_step(Step::Swapping);
        assert!(!m.can_cancel());
        assert!(!m.buttons_for(PageId::Progress).cancel);

        // Cancel is re-enabled during model download (cancels only the download).
        m.on_step(Step::DownloadingModels);
        assert!(m.can_cancel());
        assert!(m.buttons_for(PageId::Progress).cancel);
        assert_eq!(
            m.buttons_for(PageId::Progress).cancel_label,
            Some(strings::BTN_STOP_DOWNLOAD)
        );

        m.on_step(Step::Done);
        assert!(!m.can_cancel());

        let fb = m.buttons_for(PageId::Finish);
        assert!(fb.finish);
        assert!(!fb.back);
        assert!(!fb.next);
        assert!(!fb.cancel);
    }

    #[test]
    fn radio_button_mapping_round_trips() {
        for mode in [ModelMode::Full, ModelMode::Minimal, ModelMode::Later] {
            let id = radio_from_model_mode(mode);
            assert_eq!(model_mode_from_radio(id), Some(mode));
        }
        assert_eq!(model_mode_from_radio(9999), None);
    }

    #[test]
    fn finish_messages_cover_success_cancel_and_common_errors() {
        let ok = format_finish_outcome(
            Action::Install,
            &Outcome::Installed {
                version: "0.3.0-dev".to_string(),
                models: ModelsResult::Complete,
            },
        );
        assert_eq!(ok.summary, "BetterRecall 0.3.0-dev is installed.");
        assert!(ok.detail.is_empty());

        let offline = format_finish_outcome(
            Action::Install,
            &Outcome::Installed {
                version: "0.3.0-dev".to_string(),
                models: ModelsResult::Offline,
            },
        );
        assert_eq!(offline.detail, strings::FINISH_MODELS_OFFLINE);

        let dl_cancelled = format_finish_outcome(
            Action::Install,
            &Outcome::Installed {
                version: "0.3.0-dev".to_string(),
                models: ModelsResult::Cancelled,
            },
        );
        assert_eq!(dl_cancelled.detail, strings::FINISH_MODELS_CANCELLED);

        let dl_failed = format_finish_outcome(
            Action::Install,
            &Outcome::Installed {
                version: "0.3.0-dev".to_string(),
                models: ModelsResult::Failed("hash mismatch".to_string()),
            },
        );
        assert!(dl_failed.detail.contains("hash mismatch"));

        // N10: the URL shrinks to its host and the repeated cause is shown once.
        let long = "transfer failed for https://huggingface.co/x/y/resolve/abc/model.onnx: io: No such host is known. (os error 11001): io: No such host is known. (os error 11001)";
        assert_eq!(
            "transfer failed for huggingface.co: io: No such host is known. (os error 11001)",
            strings::short_failure_reason(long)
        );
        assert_eq!("hash mismatch", strings::short_failure_reason("hash mismatch"));

        let cancelled = format_finish_error(&InstallError::Cancelled);
        assert_eq!(cancelled.header, strings::FINISH_HEADER_CANCELLED);
        assert!(cancelled.summary.contains("Nothing was changed"));

        let in_use = format_finish_error(&InstallError::InUse {
            path: PathBuf::from(r"C:\install"),
        });
        assert_eq!(in_use.detail, strings::HINT_IN_USE);

        let space = format_finish_error(&InstallError::NotEnoughSpace {
            needed: 2 * 1_073_741_824,
            available: 1_073_741_824,
        });
        assert!(space.detail.contains("2.00 GB"));
        assert!(space.detail.contains("1.00 GB"));

        let downgrade = format_finish_error(&InstallError::Downgrade {
            installed: "0.3.1".to_string(),
            incoming: "0.3.0".to_string(),
        });
        assert!(downgrade.detail.contains("0.3.1"));
        assert!(downgrade.detail.contains("0.3.0"));
    }

    #[test]
    fn scale_progress_bounds() {
        assert_eq!(scale_progress(0, 1000), 0);
        assert_eq!(scale_progress(500, 1000), 500);
        assert_eq!(scale_progress(1000, 1000), 1000);
        assert_eq!(scale_progress(1500, 1000), 1000);
        assert_eq!(scale_progress(0, 0), 1000);
    }

    #[test]
    fn catalog_sizes_and_download_progress_formatting() {
        use std::time::Duration;

        let cat = br_fetch::Catalog::embedded().expect("embedded catalog");
        assert_eq!(
            strings::model_mode_radio_label(ModelMode::Full, Some(&cat)),
            "Full (about 6.6 GB)"
        );
        assert_eq!(
            strings::model_mode_radio_label(ModelMode::Minimal, Some(&cat)),
            "Minimal (about 3.5 GB)"
        );
        assert_eq!(
            strings::model_mode_radio_label(ModelMode::Later, Some(&cat)),
            "Install models later"
        );

        let prog = strings::format_download_progress(
            50_000_000,
            200_000_000,
            Duration::from_secs(2),
        );
        assert!(prog.contains("50.0 MB / 200.0 MB"));
        assert!(prog.contains("25.0 MB/s"));

        // C2: removal notice lists component IDs and size.
        let notice = strings::modify_removal_notice(
            &["whisper-large-v3-turbo", "siglip2-base"],
            2_900_000_000,
        );
        assert!(notice.contains("whisper-large-v3-turbo"));
        assert!(notice.contains("siglip2-base"));
        assert!(notice.contains("2.9 GB"));

        // F4: RemovingModels disables Cancel on Progress page.
        let mut m = WizardModel::new(WizardInit {
            action: Action::Modify,
            ..Default::default()
        });
        m.on_step(Step::DownloadingModels);
        assert!(m.can_cancel());
        m.on_step(Step::RemovingModels);
        assert!(!m.can_cancel());

        // N2: Uninstalled with data_folder_kept names the folder in detail.
        let kept = format_finish_outcome(
            Action::Uninstall,
            &Outcome::Uninstalled {
                reboot_required: false,
                data_folder_kept: Some(PathBuf::from(r"C:\dev\home")),
            },
        );
        assert!(kept.detail.contains(r"C:\dev\home"));
    }
}
