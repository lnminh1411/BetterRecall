// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! BetterRecall offline per-user installer library (`br-installer`, stage I1).

pub mod cli;
pub mod ctx;
pub mod error;
pub mod extract;
pub mod install;
pub mod log;
pub mod payload;
pub mod registry;
pub mod running;
pub mod shortcut;
pub mod swap;

pub use cli::{Action, CliArgs, parse_args, run_main};
pub use ctx::{Ctx, TEST_REG_PREFIX};
pub use error::{InstallError, exit_code};
pub use extract::{
    INSTALL_MARKER, InstallMarker, STAGING_PREFIX, Staged, available_disk_space,
    extract_to_staging, extract_to_staging_with_space, read_install_marker,
};
pub use install::{
    InstallOpts, Outcome, SETUP_EXE, SETUP_MUTEX, UninstallOpts, compare_versions, install,
    install_with_space, modify, uninstall,
};
pub use log::log_line;
pub use payload::{
    SETUP_JSON, SetupManifest, SliceReader, TRAILER_LEN, TRAILER_MAGIC, Trailer, append_payload,
    open_payload_reader, read_setup_manifest, read_trailer, stub_len, verify,
};
pub use registry::{
    APP_SUBKEY, DISPLAY_NAME, PUBLISHER, RUN_SUBKEY, UNINSTALL_SUBKEY, URL_INFO_ABOUT,
    delete_key_tree, read_reg_dword, read_reg_sz, register, unregister, write_reg_sz,
};
pub use running::{ProcInfo, close_running, pick_processes_to_end};
pub use shortcut::{
    SHORTCUT_NAME, create_shortcut, read_shortcut_target, remove_shortcut, shortcut_path,
};
pub use swap::{OLD_PREFIX, swap_into_place};
