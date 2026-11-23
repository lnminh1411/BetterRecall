// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Pinned Hugging Face model catalog embedded from `models-catalog.json`.

use std::collections::HashSet;
use std::path::{Component as PathComponent, Path};

use serde::{Deserialize, Serialize};

use crate::error::FetchError;

const EMBEDDED_CATALOG_JSON: &str = include_str!("../models-catalog.json");

/// Which predefined set of models to install.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelSet {
    Full,
    Minimal,
}

impl ModelSet {
    /// Canonical lowercase set name matching `models-catalog.json`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::Minimal => "minimal",
        }
    }
}

/// Pinned catalog of all converted BetterRecall models at a single Hugging Face commit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Catalog {
    pub repo: String,
    pub revision: String,
    #[serde(rename = "baseUrl")]
    pub base_url: String,
    pub components: Vec<Component>,
}

/// One model component folder (e.g. `embeddinggemma-2`, `ocr`, `silero-vad`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Component {
    pub id: String,
    pub sets: Vec<String>,
    pub bytes: u64,
    pub files: Vec<CatalogFile>,
}

/// One file inside a component folder with its expected size and SHA-256 digest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogFile {
    pub path: String,
    pub size: u64,
    pub sha256: String,
}

impl Catalog {
    /// Parses and validates the embedded `models-catalog.json`.
    pub fn embedded() -> Result<Self, FetchError> {
        Self::from_json(EMBEDDED_CATALOG_JSON)
    }

    /// Parses and validates a catalog from JSON text.
    pub fn from_json(raw: &str) -> Result<Self, FetchError> {
        let catalog: Self = serde_json::from_str(raw)
            .map_err(|e| FetchError::Catalog(format!("failed to parse catalog JSON: {e}")))?;
        catalog.validate()?;
        Ok(catalog)
    }

    /// Validates all catalog invariants: pinned revision, Hugging Face HTTPS base URL,
    /// safe relative paths, 64-char lowercase SHA-256 hashes, and `manifest.json` per component.
    pub fn validate(&self) -> Result<(), FetchError> {
        if self.repo.is_empty()
            || self.repo.contains("..")
            || self.repo.chars().any(char::is_whitespace)
            || !self.repo.contains('/')
        {
            return Err(FetchError::Catalog(format!(
                "invalid repo '{}'",
                self.repo
            )));
        }

        if self.revision.len() != 40 || !self.revision.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(FetchError::Catalog(format!(
                "revision '{}' must be 40 hex characters",
                self.revision
            )));
        }

        let expected_base = format!(
            "https://huggingface.co/{}/resolve/{}/",
            self.repo, self.revision
        );
        if self.base_url != expected_base {
            return Err(FetchError::Catalog(format!(
                "baseUrl '{}' does not match expected '{expected_base}'",
                self.base_url
            )));
        }

        let mut seen_ids = HashSet::new();
        for comp in &self.components {
            validate_component_id(&comp.id)?;
            if !seen_ids.insert(comp.id.as_str()) {
                return Err(FetchError::Catalog(format!(
                    "duplicate component id '{}'",
                    comp.id
                )));
            }

            let mut has_manifest = false;
            let mut seen_paths = HashSet::new();
            for file in &comp.files {
                validate_rel_path(&file.path)?;
                if !seen_paths.insert(file.path.as_str()) {
                    return Err(FetchError::Catalog(format!(
                        "duplicate file path '{}' in component '{}'",
                        file.path, comp.id
                    )));
                }
                if !is_sha256_lower_hex(&file.sha256) {
                    return Err(FetchError::Catalog(format!(
                        "invalid sha256 '{}' for {}/{}",
                        file.sha256, comp.id, file.path
                    )));
                }
                if file.path == "manifest.json" {
                    has_manifest = true;
                }
            }

            if !has_manifest {
                return Err(FetchError::Catalog(format!(
                    "component '{}' is missing manifest.json",
                    comp.id
                )));
            }
        }

        Ok(())
    }

    /// Returns all components that belong to `mode`.
    pub fn components_for(&self, mode: ModelSet) -> Vec<&Component> {
        let target = mode.as_str();
        self.components
            .iter()
            .filter(|c| c.sets.iter().any(|s| s == target))
            .collect()
    }

    /// Returns the total byte size of all components in `mode`.
    pub fn bytes_for(&self, mode: ModelSet) -> u64 {
        self.components_for(mode)
            .into_iter()
            .fold(0u64, |acc, c| acc.saturating_add(c.bytes))
    }

    /// Looks up a component by ID.
    pub fn component(&self, id: &str) -> Option<&Component> {
        self.components.iter().find(|c| c.id == id)
    }

    /// Replaces `base_url` so tests can point downloads at a local HTTP server.
    pub fn with_base_url_for_test(mut self, url: impl Into<String>) -> Self {
        let mut s = url.into();
        if !s.ends_with('/') {
            s.push('/');
        }
        self.base_url = s;
        self
    }

    /// Builds the download URL for a file inside a component.
    pub fn file_url(&self, component_id: &str, file_path: &str) -> String {
        join_base_url(&self.base_url, component_id, file_path)
    }
}

pub(crate) fn join_base_url(base_url: &str, component_id: &str, file_path: &str) -> String {
    if base_url.ends_with('/') {
        format!("{base_url}{component_id}/{file_path}")
    } else {
        format!("{base_url}/{component_id}/{file_path}")
    }
}

pub(crate) fn validate_component_id(id: &str) -> Result<(), FetchError> {
    if id.is_empty()
        || id == "."
        || id == ".."
        || id.contains('/')
        || id.contains('\\')
        || id.contains(':')
        || id.contains('\0')
    {
        return Err(FetchError::Catalog(format!("invalid component id '{id}'")));
    }
    let mut comps = Path::new(id).components();
    match (comps.next(), comps.next()) {
        (Some(PathComponent::Normal(_)), None) => Ok(()),
        _ => Err(FetchError::Catalog(format!("invalid component id '{id}'"))),
    }
}

pub(crate) fn validate_rel_path(path: &str) -> Result<(), FetchError> {
    if path.is_empty()
        || path.starts_with('/')
        || path.ends_with('/')
        || path.contains('\\')
        || path.contains(':')
        || path.contains('\0')
    {
        return Err(FetchError::Catalog(format!("invalid catalog path '{path}'")));
    }
    for seg in path.split('/') {
        if seg.is_empty() || seg == "." || seg == ".." {
            return Err(FetchError::Catalog(format!("invalid catalog path '{path}'")));
        }
    }
    let p = Path::new(path);
    if p.is_absolute() {
        return Err(FetchError::Catalog(format!("invalid catalog path '{path}'")));
    }
    let mut count = 0usize;
    for comp in p.components() {
        if !matches!(comp, PathComponent::Normal(_)) {
            return Err(FetchError::Catalog(format!("invalid catalog path '{path}'")));
        }
        count += 1;
    }
    if count == 0 {
        return Err(FetchError::Catalog(format!("invalid catalog path '{path}'")));
    }
    Ok(())
}

pub(crate) fn is_sha256_lower_hex(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
