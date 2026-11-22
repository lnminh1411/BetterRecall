// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Resumable, SHA-256-verified blocking HTTPS downloader built on `ureq` 3 + SChannel (`native-tls`).

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};
use windows::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
use windows::core::PCWSTR;

use crate::catalog::{
    Catalog, Component, join_base_url, validate_component_id, validate_rel_path,
};
use crate::error::FetchError;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const READ_STALL_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_REDIRECTS: usize = 5;
const IO_BUF_LEN: usize = 1024 * 1024;
/// Slowest transfer still accepted when sizing a file's body budget. ureq 3 has no idle timeout per read,
/// so each request gets a total budget for its body (60 s plus the remaining bytes at this rate); a stalled
/// connection then ends as a transfer error and the next attempt resumes from the `.part` file.
const MIN_BYTES_PER_SEC: u64 = 32 * 1024;
const DEFAULT_RETRY_DELAYS: [Duration; 3] = [
    Duration::from_secs(2),
    Duration::from_secs(4),
    Duration::from_secs(8),
];

/// Injectable free-space query callback.
pub type FreeSpaceFn = Box<dyn Fn(&Path) -> Result<u64, FetchError>>;

/// Byte-level progress sink for model downloads.
pub trait FetchProgress {
    /// Called once up front with the total number of bytes to download.
    fn total(&self, bytes: u64);
    /// Called as each chunk of bytes arrives from the server.
    fn bytes(&self, delta: u64);
    /// Called before each file starts downloading with `<component_id>/<rel_path>`.
    fn file(&self, _rel_path: &str) {}
}

/// No-op [`FetchProgress`] implementation.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoFetchProgress;

impl FetchProgress for NoFetchProgress {
    fn total(&self, _bytes: u64) {}
    fn bytes(&self, _delta: u64) {}
}

/// Installed state of one catalog component under a models directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComponentState {
    Missing,
    Partial,
    Complete,
}

/// Summary returned by [`install_components`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct InstallReport {
    pub downloaded_files: usize,
    pub skipped_files: usize,
    pub bytes_downloaded: u64,
}

/// Options for [`install_components`].
pub struct InstallOptions {
    pub base_url: String,
    pub downloader: Downloader,
    pub free_space: FreeSpaceFn,
}

impl InstallOptions {
    /// Creates options targeting `catalog.base_url` with the default [`Downloader`] and disk check.
    pub fn for_catalog(catalog: &Catalog) -> Self {
        Self {
            base_url: catalog.base_url.clone(),
            downloader: Downloader::new(),
            free_space: Box::new(available_disk_space),
        }
    }

    /// Replaces the [`Downloader`] (e.g. shorter retry delays in tests).
    pub fn with_downloader(mut self, downloader: Downloader) -> Self {
        self.downloader = downloader;
        self
    }

    /// Replaces the free-space probe (for tests).
    pub fn with_free_space<F>(mut self, f: F) -> Self
    where
        F: Fn(&Path) -> Result<u64, FetchError> + 'static,
    {
        self.free_space = Box::new(f);
        self
    }
}

/// Blocking HTTPS downloader with resume, SHA-256 verification, retries, and redirect checks.
#[derive(Clone)]
pub struct Downloader {
    agent: ureq::Agent,
    user_agent: String,
    retry_delays: Vec<Duration>,
}

impl Default for Downloader {
    fn default() -> Self {
        Self::new()
    }
}

impl Downloader {
    /// Creates a downloader with 30 s connect timeout, 60 s read/response stall timeout,
    /// SChannel (`native-tls`), manual HTTPS-only redirects (up to 5), and 3 retries (2 s, 4 s, 8 s).
    pub fn new() -> Self {
        let config = ureq::Agent::config_builder()
            .max_redirects(0)
            .http_status_as_error(false)
            .timeout_connect(Some(CONNECT_TIMEOUT))
            .timeout_send_request(Some(READ_STALL_TIMEOUT))
            .timeout_recv_response(Some(READ_STALL_TIMEOUT))
            .tls_config(
                ureq::tls::TlsConfig::builder()
                    .provider(ureq::tls::TlsProvider::NativeTls)
                    .build(),
            )
            .build();
        let agent = ureq::Agent::new_with_config(config);
        let user_agent = format!("BetterRecall/{}", env!("CARGO_PKG_VERSION"));
        Self {
            agent,
            user_agent,
            retry_delays: DEFAULT_RETRY_DELAYS.to_vec(),
        }
    }

    /// Overrides the retry backoff schedule (primarily for fast unit/integration tests).
    pub fn with_retry_delays(mut self, delays: Vec<Duration>) -> Self {
        self.retry_delays = delays;
        self
    }

    /// Downloads `url` to `dest` via `<dest>.part`, resuming if a partial file exists, verifying
    /// exact size and SHA-256 before atomically renaming `.part` into `dest`.
    pub fn download_file(
        &self,
        url: &str,
        dest: &Path,
        expected_size: u64,
        expected_sha256: &str,
        progress: &dyn FetchProgress,
        cancel: &dyn Fn() -> bool,
    ) -> Result<(), FetchError> {
        if !is_allowed_url(url) {
            return Err(FetchError::Http {
                status: 0,
                url: url.to_string(),
            });
        }
        let expected_hash = decode_sha256_hex(expected_sha256).ok_or_else(|| {
            FetchError::Catalog(format!("invalid expected sha256 '{expected_sha256}'"))
        })?;

        if let Some(parent) = dest.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent).map_err(|e| {
                FetchError::io(format!("creating directory {}", parent.display()), e)
            })?;
        }

        let part = part_path(dest);
        let mut received_any_byte = false;
        let mut retried_416 = false;
        let mut attempt = 0usize;

        loop {
            if cancel() {
                return Err(FetchError::Cancelled);
            }

            let mut resume_from = existing_part_len(&part)?;
            if resume_from > expected_size {
                remove_file_if_exists(&part)?;
                resume_from = 0;
            } else if resume_from == expected_size && expected_size > 0 && part.exists() {
                // Previous run finished writing `.part` before renaming.
                let actual = hash_file(&part, cancel)?;
                if ct_eq_32(&actual, &expected_hash) {
                    fs::rename(&part, dest).map_err(|e| {
                        FetchError::io(
                            format!("renaming {} to {}", part.display(), dest.display()),
                            e,
                        )
                    })?;
                    return Ok(());
                }
                remove_file_if_exists(&part)?;
                resume_from = 0;
            }

            let range_header = if resume_from > 0 {
                Some(format!("bytes={resume_from}-"))
            } else {
                None
            };

            let body_budget = READ_STALL_TIMEOUT
                + Duration::from_secs(expected_size.saturating_sub(resume_from) / MIN_BYTES_PER_SEC);
            let resp = match self.send_following_redirects(url, range_header.as_deref(), body_budget) {
                Ok(r) => r,
                Err(SendErr::Connect) => {
                    if !received_any_byte && attempt == 0 {
                        return Err(FetchError::Offline);
                    }
                    if let Some(&delay) = self.retry_delays.get(attempt) {
                        sleep_cancellable(delay, cancel)?;
                        attempt += 1;
                        continue;
                    }
                    return Err(FetchError::Offline);
                }
                Err(SendErr::Transient(msg)) => {
                    if let Some(&delay) = self.retry_delays.get(attempt) {
                        sleep_cancellable(delay, cancel)?;
                        attempt += 1;
                        continue;
                    }
                    return Err(FetchError::io(
                        format!("transfer failed for {url}: {msg}"),
                        io::Error::other(msg),
                    ));
                }
                Err(SendErr::Fatal(e)) => return Err(e),
            };

            let status = resp.status().as_u16();
            if status == 416 && !retried_416 {
                retried_416 = true;
                remove_file_if_exists(&part)?;
                continue;
            }
            if (500..600).contains(&status) {
                if let Some(&delay) = self.retry_delays.get(attempt) {
                    sleep_cancellable(delay, cancel)?;
                    attempt += 1;
                    continue;
                }
                return Err(FetchError::Http {
                    status,
                    url: url.to_string(),
                });
            }
            if status != 200 && status != 206 {
                return Err(FetchError::Http {
                    status,
                    url: url.to_string(),
                });
            }

            let appending = status == 206 && resume_from > 0;
            let mut hasher = Sha256::new();
            let mut written: u64 = 0;

            let mut out = if appending {
                hash_existing_part(&part, &mut hasher, cancel)?;
                written = resume_from;
                OpenOptions::new()
                    .append(true)
                    .open(&part)
                    .map_err(|e| FetchError::io(format!("opening {}", part.display()), e))?
            } else {
                File::create(&part)
                    .map_err(|e| FetchError::io(format!("creating {}", part.display()), e))?
            };

            // The body is read on its own thread so a stalled connection cannot block this loop:
            // Cancel is honoured within a quarter second, and 60 s without data counts as a failed
            // transfer (retried, resuming from the part). A reader left blocked ends when the
            // request's body budget runs out.
            let chunks = spawn_body_reader(resp.into_body().into_reader());
            let mut transfer_err: Option<io::Error> = None;
            let mut last_data = Instant::now();

            loop {
                if cancel() {
                    let _ = out.flush();
                    return Err(FetchError::Cancelled);
                }
                match chunks.recv_timeout(Duration::from_millis(250)) {
                    Ok(Ok(chunk)) => {
                        last_data = Instant::now();
                        received_any_byte = true;
                        hasher.update(&chunk);
                        out.write_all(&chunk).map_err(|e| {
                            FetchError::io(format!("writing {}", part.display()), e)
                        })?;
                        written = written.saturating_add(chunk.len() as u64);
                        progress.bytes(chunk.len() as u64);
                    }
                    Ok(Err(e)) => {
                        transfer_err = Some(e);
                        break;
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        if last_data.elapsed() >= READ_STALL_TIMEOUT {
                            transfer_err = Some(io::Error::new(
                                io::ErrorKind::TimedOut,
                                "no data for 60 s",
                            ));
                            break;
                        }
                    }
                    // The reader finished: end of body.
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }

            out.flush()
                .map_err(|e| FetchError::io(format!("flushing {}", part.display()), e))?;
            drop(out);

            if let Some(err) = transfer_err {
                // Keep `.part` for resume on the next retry or caller invocation.
                if let Some(&delay) = self.retry_delays.get(attempt) {
                    sleep_cancellable(delay, cancel)?;
                    attempt += 1;
                    continue;
                }
                return Err(FetchError::io(format!("downloading {url}"), err));
            }

            let digest: [u8; 32] = hasher.finalize().into();
            if written != expected_size || !ct_eq_32(&digest, &expected_hash) {
                let _ = remove_file_if_exists(&part);
                return Err(FetchError::Verify {
                    path: dest.to_path_buf(),
                });
            }

            fs::rename(&part, dest).map_err(|e| {
                FetchError::io(
                    format!("renaming {} to {}", part.display(), dest.display()),
                    e,
                )
            })?;
            return Ok(());
        }
    }

    fn send_following_redirects(
        &self,
        initial_url: &str,
        range_header: Option<&str>,
        body_budget: Duration,
    ) -> Result<ureq::http::Response<ureq::Body>, SendErr> {
        let mut current = initial_url.to_string();
        for hop in 0..=MAX_REDIRECTS {
            if !is_allowed_url(&current) {
                return Err(SendErr::Fatal(FetchError::Http {
                    status: 0,
                    url: current,
                }));
            }
            let mut req = self
                .agent
                .get(&current)
                .config()
                .timeout_recv_body(Some(body_budget))
                .build()
                .header("User-Agent", &self.user_agent);
            if let Some(range) = range_header {
                req = req.header("Range", range);
            }
            let resp = req.call().map_err(classify_ureq_error)?;
            let status = resp.status().as_u16();
            if matches!(status, 301 | 302 | 303 | 307 | 308) {
                if hop == MAX_REDIRECTS {
                    return Err(SendErr::Fatal(FetchError::Http {
                        status,
                        url: current,
                    }));
                }
                let location = resp
                    .headers()
                    .get("location")
                    .and_then(|v| v.to_str().ok())
                    .ok_or_else(|| {
                        SendErr::Fatal(FetchError::Http {
                            status,
                            url: current.clone(),
                        })
                    })?;
                let next = resolve_redirect(&current, location).ok_or_else(|| {
                    SendErr::Fatal(FetchError::Http {
                        status,
                        url: location.to_string(),
                    })
                })?;
                if !is_allowed_url(&next) {
                    return Err(SendErr::Fatal(FetchError::Http {
                        status,
                        url: next,
                    }));
                }
                current = next;
                continue;
            }
            return Ok(resp);
        }
        Err(SendErr::Fatal(FetchError::Http {
            status: 302,
            url: current,
        }))
    }
}

/// Reads the body into chunks on a separate thread. The channel closes at the end of the body; a
/// read error is sent as the last item.
fn spawn_body_reader<R: Read + Send + 'static>(mut reader: R) -> mpsc::Receiver<io::Result<Vec<u8>>> {
    let (tx, rx) = mpsc::sync_channel::<io::Result<Vec<u8>>>(4);
    thread::Builder::new()
        .name("br-fetch body".into())
        .spawn(move || {
            loop {
                let mut buf = vec![0u8; IO_BUF_LEN];
                match reader.read(&mut buf) {
                    Ok(0) => return,
                    Ok(n) => {
                        buf.truncate(n);
                        if tx.send(Ok(buf)).is_err() {
                            return; // the caller gave up (cancel or stall)
                        }
                    }
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    Err(e) => {
                        let _ = tx.send(Err(e));
                        return;
                    }
                }
            }
        })
        .expect("spawning the download body reader");
    rx
}

enum SendErr {
    Connect,
    Transient(String),
    Fatal(FetchError),
}

fn classify_ureq_error(err: ureq::Error) -> SendErr {
    match err {
        ureq::Error::HostNotFound
        | ureq::Error::ConnectionFailed
        | ureq::Error::Tls(_)
        | ureq::Error::Timeout(ureq::Timeout::Connect | ureq::Timeout::Resolve) => SendErr::Connect,
        ureq::Error::Io(ref io_err)
            if matches!(
                io_err.kind(),
                io::ErrorKind::ConnectionRefused
                    | io::ErrorKind::NetworkUnreachable
                    | io::ErrorKind::HostUnreachable
                    | io::ErrorKind::AddrNotAvailable
                    | io::ErrorKind::NotConnected
            ) =>
        {
            SendErr::Connect
        }
        other => SendErr::Transient(other.to_string()),
    }
}

/// Checks whether a URL is allowed by `br-fetch`: `https://` for any host,
/// or `http://` strictly for `127.0.0.1` or `localhost` (for local tests).
pub fn is_allowed_url(url: &str) -> bool {
    if let Some(rest) = url.strip_prefix("https://") {
        return !extract_authority_host(rest).is_empty();
    }
    if let Some(rest) = url.strip_prefix("http://") {
        let host = extract_authority_host(rest);
        return host == "127.0.0.1" || host.eq_ignore_ascii_case("localhost");
    }
    false
}

fn extract_authority_host(after_scheme: &str) -> &str {
    let authority = after_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or("");
    let host_port = authority
        .rsplit_once('@')
        .map_or(authority, |(_, hp)| hp);
    if host_port.starts_with('[') {
        // IPv6 literal.
        host_port.split(']').next().unwrap_or("")
    } else {
        host_port.split(':').next().unwrap_or("")
    }
}

fn resolve_redirect(base: &str, location: &str) -> Option<String> {
    if location.starts_with("http://") || location.starts_with("https://") {
        return Some(location.to_string());
    }
    let scheme_end = base.find("://")?;
    let scheme = &base[..scheme_end];
    let after_scheme = &base[scheme_end + 3..];
    if let Some(rest) = location.strip_prefix("//") {
        return Some(format!("{scheme}://{rest}"));
    }
    let (authority, base_path) = match after_scheme.find('/') {
        Some(idx) => (&after_scheme[..idx], &after_scheme[idx..]),
        None => (after_scheme, "/"),
    };
    if location.starts_with('/') {
        Some(format!("{scheme}://{authority}{location}"))
    } else {
        let dir = match base_path.rfind('/') {
            Some(idx) => &base_path[..=idx],
            None => "/",
        };
        Some(format!("{scheme}://{authority}{dir}{location}"))
    }
}

/// Downloads and verifies all files for `components` into `<models_dir>\<id>\`.
///
/// Existing verified files are skipped; `manifest.json` is downloaded last for each component
/// so partially downloaded components stay invisible to the worker. Free space for the bytes
/// still to download plus 5% is checked up front.
pub fn install_components(
    models_dir: &Path,
    components: &[&Component],
    opts: &InstallOptions,
    progress: &dyn FetchProgress,
    cancel: &dyn Fn() -> bool,
) -> Result<InstallReport, FetchError> {
    if cancel() {
        return Err(FetchError::Cancelled);
    }

    struct PendingFile<'a> {
        component_id: &'a str,
        rel_path: &'a str,
        size: u64,
        sha256: &'a str,
        dest: PathBuf,
        remaining: u64,
    }

    let mut pending: Vec<PendingFile<'_>> = Vec::new();
    let mut skipped_files = 0usize;
    let mut bytes_to_fetch = 0u64;

    for comp in components {
        validate_component_id(&comp.id)?;
        let comp_dir = models_dir.join(&comp.id);

        // Order non-manifest files first, `manifest.json` last.
        let ordered = comp
            .files
            .iter()
            .filter(|f| f.path != "manifest.json")
            .chain(comp.files.iter().filter(|f| f.path == "manifest.json"));

        for file in ordered {
            if cancel() {
                return Err(FetchError::Cancelled);
            }
            validate_rel_path(&file.path)?;
            let dest = comp_dir.join(&file.path);
            if file_matches_catalog(&dest, file.size, &file.sha256, cancel)? {
                skipped_files += 1;
                continue;
            }
            let part_len = existing_part_len(&part_path(&dest))?;
            let remaining = if part_len < file.size {
                file.size - part_len
            } else {
                file.size
            };
            bytes_to_fetch = bytes_to_fetch.saturating_add(remaining);
            pending.push(PendingFile {
                component_id: &comp.id,
                rel_path: &file.path,
                size: file.size,
                sha256: &file.sha256,
                dest,
                remaining,
            });
        }
    }

    if bytes_to_fetch > 0 {
        let needed = bytes_to_fetch.saturating_add(bytes_to_fetch.div_ceil(20));
        let available = (opts.free_space)(models_dir)?;
        if available < needed {
            return Err(FetchError::NotEnoughSpace { needed, available });
        }
    }

    progress.total(bytes_to_fetch);

    let mut downloaded_files = 0usize;
    let mut bytes_downloaded = 0u64;
    for item in pending {
        if cancel() {
            return Err(FetchError::Cancelled);
        }
        let url = join_base_url(&opts.base_url, item.component_id, item.rel_path);
        progress.file(&format!("{}/{}", item.component_id, item.rel_path));
        opts.downloader.download_file(
            &url,
            &item.dest,
            item.size,
            item.sha256,
            progress,
            cancel,
        )?;
        downloaded_files += 1;
        bytes_downloaded = bytes_downloaded.saturating_add(item.remaining);
    }

    Ok(InstallReport {
        downloaded_files,
        skipped_files,
        bytes_downloaded,
    })
}

/// Removes `<models_dir>\<id>\` for each id in `ids`, validating against the embedded catalog.
/// Refuses any id not in the catalog or any path that could leave `models_dir`.
pub fn remove_components(models_dir: &Path, ids: &[&str]) -> Result<(), FetchError> {
    let catalog = Catalog::embedded()?;
    remove_components_from_catalog(models_dir, &catalog, ids)
}

/// Same as [`remove_components`], validating against an explicit `catalog` (useful in tests).
pub fn remove_components_from_catalog(
    models_dir: &Path,
    catalog: &Catalog,
    ids: &[&str],
) -> Result<(), FetchError> {
    // Validate every id before deleting anything.
    for &id in ids {
        validate_component_id(id)?;
        if catalog.component(id).is_none() {
            return Err(FetchError::Catalog(format!(
                "refusing to remove unknown component id '{id}'"
            )));
        }
    }

    for &id in ids {
        let target = models_dir.join(id);
        if target.parent() != Some(models_dir) {
            return Err(FetchError::Catalog(format!(
                "refusing to remove path outside models_dir: '{}'",
                target.display()
            )));
        }
        match fs::remove_dir_all(&target) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(FetchError::io(
                    format!("removing component folder {}", target.display()),
                    e,
                ));
            }
        }
    }
    Ok(())
}

/// Inspects `<models_dir>` and returns `(component_id, ComponentState)` for every catalog entry.
///
/// `Complete` means `manifest.json` is present with the exact size and SHA-256 from the catalog;
/// large model weight files are not hashed here.
pub fn installed_state(models_dir: &Path, catalog: &Catalog) -> Vec<(String, ComponentState)> {
    catalog
        .components
        .iter()
        .map(|comp| {
            let state = component_state(models_dir, comp);
            (comp.id.clone(), state)
        })
        .collect()
}

fn component_state(models_dir: &Path, comp: &Component) -> ComponentState {
    let comp_dir = models_dir.join(&comp.id);
    if !comp_dir.is_dir() {
        return ComponentState::Missing;
    }

    if let Some(manifest) = comp.files.iter().find(|f| f.path == "manifest.json") {
        let manifest_path = comp_dir.join("manifest.json");
        if file_matches_catalog(&manifest_path, manifest.size, &manifest.sha256, &|| false)
            .unwrap_or(false)
        {
            return ComponentState::Complete;
        }
    }

    let has_any_entry = fs::read_dir(&comp_dir)
        .ok()
        .and_then(|mut it| it.next())
        .is_some();
    if has_any_entry {
        ComponentState::Partial
    } else {
        ComponentState::Missing
    }
}

/// Queries available free bytes on the volume containing `path` (walking up to the nearest
/// existing ancestor).
pub fn available_disk_space(path: &Path) -> Result<u64, FetchError> {
    let mut cur = Some(path);
    let existing = loop {
        match cur {
            Some(p) if p.exists() => break p.to_path_buf(),
            Some(p) => cur = p.parent(),
            None => break path.to_path_buf(),
        }
    };
    let wide: Vec<u16> = existing.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut free_available = 0u64;
    unsafe {
        GetDiskFreeSpaceExW(
            PCWSTR(wide.as_ptr()),
            Some(&mut free_available),
            None,
            None,
        )
    }
    .map_err(|e| {
        FetchError::io(
            format!("checking free disk space for {}", existing.display()),
            io::Error::from_raw_os_error(e.code().0),
        )
    })?;
    Ok(free_available)
}

fn part_path(dest: &Path) -> PathBuf {
    let mut os = dest.as_os_str().to_os_string();
    os.push(".part");
    PathBuf::from(os)
}

fn existing_part_len(part: &Path) -> Result<u64, FetchError> {
    match fs::metadata(part) {
        Ok(meta) => Ok(meta.len()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(0),
        Err(e) => Err(FetchError::io(
            format!("reading metadata for {}", part.display()),
            e,
        )),
    }
}

fn remove_file_if_exists(path: &Path) -> Result<(), FetchError> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(FetchError::io(format!("removing {}", path.display()), e)),
    }
}

fn file_matches_catalog(
    path: &Path,
    expected_size: u64,
    expected_sha256: &str,
    cancel: &dyn Fn() -> bool,
) -> Result<bool, FetchError> {
    let meta = match fs::metadata(path) {
        Ok(m) if m.is_file() => m,
        Ok(_) => return Ok(false),
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(e) => {
            return Err(FetchError::io(
                format!("reading metadata for {}", path.display()),
                e,
            ));
        }
    };
    if meta.len() != expected_size {
        return Ok(false);
    }
    let Some(expected) = decode_sha256_hex(expected_sha256) else {
        return Ok(false);
    };
    let actual = hash_file(path, cancel)?;
    Ok(ct_eq_32(&actual, &expected))
}

fn hash_file(path: &Path, cancel: &dyn Fn() -> bool) -> Result<[u8; 32], FetchError> {
    let mut hasher = Sha256::new();
    hash_existing_part(path, &mut hasher, cancel)?;
    Ok(hasher.finalize().into())
}

fn hash_existing_part(
    path: &Path,
    hasher: &mut Sha256,
    cancel: &dyn Fn() -> bool,
) -> Result<(), FetchError> {
    let mut file =
        File::open(path).map_err(|e| FetchError::io(format!("opening {}", path.display()), e))?;
    let mut buf = vec![0u8; IO_BUF_LEN];
    loop {
        if cancel() {
            return Err(FetchError::Cancelled);
        }
        let n = file
            .read(&mut buf)
            .map_err(|e| FetchError::io(format!("reading {}", path.display()), e))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(())
}

fn decode_sha256_hex(hex: &str) -> Option<[u8; 32]> {
    if hex.len() != 64 {
        return None;
    }
    let bytes = hex.as_bytes();
    let mut out = [0u8; 32];
    for (i, slot) in out.iter_mut().enumerate() {
        let hi = hex_nibble(bytes[i * 2])?;
        let lo = hex_nibble(bytes[i * 2 + 1])?;
        *slot = (hi << 4) | lo;
    }
    Some(out)
}

fn hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        _ => None,
    }
}

fn ct_eq_32(a: &[u8; 32], b: &[u8; 32]) -> bool {
    let mut diff = 0u8;
    for i in 0..32 {
        diff |= a[i] ^ b[i];
    }
    diff == 0
}

fn sleep_cancellable(delay: Duration, cancel: &dyn Fn() -> bool) -> Result<(), FetchError> {
    let deadline = Instant::now() + delay;
    while Instant::now() < deadline {
        if cancel() {
            return Err(FetchError::Cancelled);
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        thread::sleep(remaining.min(Duration::from_millis(50)));
    }
    if cancel() {
        return Err(FetchError::Cancelled);
    }
    Ok(())
}
