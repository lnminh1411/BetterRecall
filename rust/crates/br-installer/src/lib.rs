// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! BetterRecall offline per-user installer library (`br-installer`, stages I1-I2).

pub mod cli;
pub mod ctx;
pub mod error;
pub mod extract;
pub mod install;
pub mod log;
pub mod payload;
pub mod progress;
pub mod registry;
pub mod running;
pub mod shortcut;
pub mod strings;
pub mod swap;
pub mod wizard;

pub use cli::{Action, CliArgs, RELAUNCHED_FLAG, parse_args, run_main};
pub use ctx::{Ctx, TEST_REG_PREFIX};
pub use error::{InstallError, exit_code};
pub use extract::{
    INSTALL_MARKER, InstallMarker, STAGING_PREFIX, Staged, available_disk_space,
    extract_to_staging, extract_to_staging_with_progress, extract_to_staging_with_space,
    read_install_marker,
};
pub use install::{
    InstallOpts, ModelMode, ModelsResult, ModifyOpts, Outcome, SETUP_EXE, SETUP_MUTEX,
    UninstallOpts, compare_versions, install, install_with_progress, install_with_space, modify,
    modify_with_progress, resolve_models_dir, uninstall, uninstall_with_progress,
    validate_delete_data_home,
};
pub use log::log_line;
pub use payload::{
    LICENSE_ENTRY, SETUP_JSON, SetupManifest, SliceReader, TRAILER_LEN, TRAILER_MAGIC, Trailer,
    append_payload, open_payload_reader, read_license_text, read_setup_manifest, read_trailer,
    stub_len, verify,
};
pub use progress::{NoProgress, Progress, Step};
pub use registry::{
    APP_SUBKEY, DISPLAY_NAME, MODEL_MODE_VALUE, PUBLISHER, RUN_SUBKEY, UNINSTALL_SUBKEY,
    URL_INFO_ABOUT, delete_key_tree, read_model_mode, read_reg_dword, read_reg_sz, register,
    unregister, write_model_mode, write_reg_sz,
};
pub use running::{ProcInfo, close_running, pick_processes_to_end};
pub use shortcut::{
    SHORTCUT_NAME, create_shortcut, read_shortcut_target, remove_shortcut, shortcut_path,
};
pub use swap::{OLD_PREFIX, swap_into_place};
