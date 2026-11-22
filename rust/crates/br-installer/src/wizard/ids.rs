// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Numeric resource and control identifiers shared between `build.rs` and `wizard::ui`.

pub const IDI_APP_ICON: u16 = 1;

// Dialog template IDs (must match `build.rs`).
pub const IDD_LICENCE: u16 = 101;
pub const IDD_MODELS: u16 = 102;
pub const IDD_READY: u16 = 103;
pub const IDD_UNINSTALL_CONFIRM: u16 = 104;
pub const IDD_PROGRESS: u16 = 105;
pub const IDD_FINISH: u16 = 106;

// Licence page controls.
pub const IDC_LICENCE_WELCOME: u16 = 1001;
pub const IDC_LICENCE_EDIT: u16 = 1002;
pub const IDC_LICENCE_THIRD_PARTY: u16 = 1003;
pub const IDC_LICENCE_ACCEPT: u16 = 1004;

// Models page controls.
pub const IDC_MODELS_FULL: u16 = 1101;
pub const IDC_MODELS_FULL_DESC: u16 = 1102;
pub const IDC_MODELS_MINIMAL: u16 = 1103;
pub const IDC_MODELS_MINIMAL_DESC: u16 = 1104;
pub const IDC_MODELS_LATER: u16 = 1105;
pub const IDC_MODELS_LATER_DESC: u16 = 1106;
pub const IDC_MODELS_NOTE: u16 = 1107;

// Ready page controls.
pub const IDC_READY_INSTALL_LABEL: u16 = 1201;
pub const IDC_READY_INSTALL_PATH: u16 = 1202;
pub const IDC_READY_DATA_LABEL: u16 = 1203;
pub const IDC_READY_DATA_PATH: u16 = 1204;
pub const IDC_READY_START_CHECK: u16 = 1205;
pub const IDC_READY_MODELS_LABEL: u16 = 1206;
pub const IDC_READY_MODELS_PATH: u16 = 1207;
pub const IDC_READY_DOWNLOAD_INFO: u16 = 1208;

// Uninstall confirm page controls.
pub const IDC_UNINSTALL_KEEP: u16 = 1301;
pub const IDC_UNINSTALL_DELETE: u16 = 1302;
pub const IDC_UNINSTALL_WARNING: u16 = 1303;
pub const IDC_UNINSTALL_UNDERSTAND: u16 = 1304;

// Progress page controls.
pub const IDC_PROGRESS_STATUS: u16 = 1401;
pub const IDC_PROGRESS_BAR: u16 = 1402;
pub const IDC_PROGRESS_DETAIL: u16 = 1403;

// Finish page controls.
pub const IDC_FINISH_SUMMARY: u16 = 1501;
pub const IDC_FINISH_DETAIL: u16 = 1502;
