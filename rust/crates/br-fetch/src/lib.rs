// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Shared blocking HTTPS model downloader and pinned catalog (`br-fetch`, stage I3).

pub mod catalog;
pub mod download;
pub mod error;

pub use catalog::{Catalog, CatalogFile, Component, ModelSet};
pub use download::{
    ComponentState, Downloader, FetchProgress, FreeSpaceFn, InstallOptions, InstallReport,
    NoFetchProgress, available_disk_space, install_components, installed_state, is_allowed_url,
    remove_components, remove_components_from_catalog,
};
pub use error::FetchError;
