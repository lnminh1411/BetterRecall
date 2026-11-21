// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Pure guardrail policy decisions (`GuardrailPolicy.cs`).

use std::path::Path;

use br_contracts::{PrivacySettings, full_path};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowInfo {
    pub hwnd: isize,
    pub pid: u32,
    pub exe_path: String,
    pub title: String,
    pub class_name: String,
}

impl WindowInfo {
    pub fn process_name(&self) -> &str {
        if self.exe_path.is_empty() {
            return "";
        }
        Path::new(&self.exe_path)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BadgeResult {
    NotChecked,
    NotBrowser,
    NoBadge,
    Private,
    CheckFailed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuardReason {
    None,
    SessionLocked,
    KeyLocked,
    NoForeground,
    DenyProcess,
    DenyPath,
    DenyTitle,
    DisplayAffinity,
    PrivateBadge,
    BadgeCheckFailed,
    ForegroundChanged,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GuardDecision {
    pub allow: bool,
    pub reason: GuardReason,
}

impl GuardDecision {
    pub const ALLOWED: Self = Self {
        allow: true,
        reason: GuardReason::None,
    };

    pub const fn deny(reason: GuardReason) -> Self {
        Self {
            allow: false,
            reason,
        }
    }
}

pub struct GuardrailPolicy;

impl GuardrailPolicy {
    const BROWSERS: &'static [&'static str] = &[
        "chrome.exe",
        "msedge.exe",
        "brave.exe",
        "vivaldi.exe",
        "opera.exe",
        "opera_gx.exe",
        "chromium.exe",
        "arc.exe",
        "thorium.exe",
    ];

    /// Host shown in a browser address bar (`"github.com/x"`, `"https://www.example.org/a?b"`),
    /// lower case, without `"www."`. Empty for search text, `chrome://`, files, or non-http(s).
    pub fn site_from_address(text: Option<&str>) -> String {
        let Some(raw) = text else {
            return String::new();
        };
        let t = raw.trim();
        if t.is_empty() || t.len() > 2048 || t.chars().any(char::is_whitespace) {
            return String::new();
        }
        let rest = if let Some(idx) = t.find("://") {
            let scheme = &t[..idx];
            if !scheme.eq_ignore_ascii_case("http") && !scheme.eq_ignore_ascii_case("https") {
                return String::new();
            }
            &t[idx + 3..]
        } else {
            // Reject schemes like about:blank, mailto:x unless it's host:port(/|$)
            if t.contains(':') && !is_host_port(t) {
                return String::new();
            }
            t
        };

        // Authority is up to '/', '?', '#'
        let auth_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
        let authority = &rest[..auth_end];
        if authority.is_empty() {
            return String::new();
        }
        // Strip optional userinfo@
        let hostport = match authority.rfind('@') {
            Some(at) => &authority[at + 1..],
            None => authority,
        };
        let host_raw = if hostport.starts_with('[') {
            // IPv6 literal
            let Some(end) = hostport.find(']') else {
                return String::new();
            };
            let after = &hostport[end + 1..];
            if !after.is_empty() && !valid_port_suffix(after) {
                return String::new();
            }
            &hostport[..=end]
        } else if let Some(colon) = hostport.find(':') {
            if !valid_port_suffix(&hostport[colon..]) {
                return String::new();
            }
            &hostport[..colon]
        } else {
            hostport
        };

        if host_raw.is_empty() {
            return String::new();
        }
        // Validate host characters (basic RFC 3986 / IDN host sanity)
        if host_raw.starts_with('.') || host_raw.ends_with('.') || host_raw.contains("..") {
            return String::new();
        }
        let host = host_raw.to_lowercase();
        let host = host.strip_prefix("www.").unwrap_or(&host);
        if host.contains('.') || host == "localhost" {
            host.to_string()
        } else {
            String::new()
        }
    }

    pub fn is_chromium_browser(process_name: &str) -> bool {
        Self::BROWSERS
            .iter()
            .any(|b| b.eq_ignore_ascii_case(process_name))
    }

    pub fn badge_hides(r: BadgeResult) -> bool {
        matches!(
            r,
            BadgeResult::Private | BadgeResult::CheckFailed | BadgeResult::NotChecked
        )
    }

    pub fn hides_browser_window<F>(w: &WindowInfo, badge: Option<F>) -> bool
    where
        F: FnOnce(isize) -> BadgeResult,
    {
        if w.title.is_empty() || !Self::is_chromium_browser(w.process_name()) {
            return false;
        }
        let res = match badge {
            Some(f) => f(w.hwnd),
            None => BadgeResult::CheckFailed,
        };
        Self::badge_hides(res)
    }

    pub fn match_deny_lists(w: &WindowInfo, p: &PrivacySettings) -> GuardReason {
        if Self::is_denied_process(&w.exe_path, &p.deny_processes) {
            return GuardReason::DenyProcess;
        }
        if Self::is_denied_path(&w.exe_path, &p.deny_path_prefixes) {
            return GuardReason::DenyPath;
        }
        if Self::title_matches(&w.title, &p.deny_title_patterns) {
            return GuardReason::DenyTitle;
        }
        GuardReason::None
    }

    pub fn evaluate(
        foreground: Option<&WindowInfo>,
        p: &PrivacySettings,
        session_locked: bool,
        any_visible_affinity: bool,
        badge: BadgeResult,
    ) -> GuardDecision {
        if session_locked && p.pause_on_lock {
            return GuardDecision::deny(GuardReason::SessionLocked);
        }
        let Some(fg) = foreground else {
            return GuardDecision::deny(GuardReason::NoForeground);
        };
        let deny = Self::match_deny_lists(fg, p);
        if deny != GuardReason::None {
            return GuardDecision::deny(deny);
        }
        if p.honor_display_affinity && any_visible_affinity {
            return GuardDecision::deny(GuardReason::DisplayAffinity);
        }
        if p.browser_badge_detection && Self::is_chromium_browser(fg.process_name()) {
            if badge == BadgeResult::Private {
                return GuardDecision::deny(GuardReason::PrivateBadge);
            }
            if matches!(badge, BadgeResult::CheckFailed | BadgeResult::NotChecked) {
                return GuardDecision::deny(GuardReason::BadgeCheckFailed);
            }
        }
        GuardDecision::ALLOWED
    }

    pub fn is_denied_process(exe_path: &str, deny_processes: &[String]) -> bool {
        if exe_path.is_empty() {
            return false;
        }
        let name = Path::new(exe_path)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("");
        for d in deny_processes {
            let entry = d.trim();
            if entry.is_empty() {
                continue;
            }
            if entry.contains('\\') || entry.contains('/') {
                if normalize_path(entry).eq_ignore_ascii_case(&normalize_path(exe_path)) {
                    return true;
                }
            } else if entry.eq_ignore_ascii_case(name)
                || format!("{entry}.exe").eq_ignore_ascii_case(name)
            {
                return true;
            }
        }
        false
    }

    pub fn is_denied_path(exe_path: &str, prefixes: &[String]) -> bool {
        if exe_path.is_empty() {
            return false;
        }
        let full = normalize_path(exe_path);
        for raw in prefixes {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                continue;
            }
            let expanded = expand_env_vars(trimmed);
            let prefix = normalize_path(&expanded);
            if prefix.is_empty() {
                continue;
            }
            if full.len() >= prefix.len()
                && full.is_char_boundary(prefix.len())
                && full[..prefix.len()].eq_ignore_ascii_case(&prefix)
                && (full.len() == prefix.len()
                    || prefix.ends_with('\\')
                    || full.as_bytes()[prefix.len()] == b'\\')
            {
                return true;
            }
        }
        false
    }

    /// Case-insensitive substring match; `*` matches any run of characters.
    pub fn title_matches(title: &str, patterns: &[String]) -> bool {
        if title.is_empty() {
            return false;
        }
        let title_lower = title.to_lowercase();
        for raw in patterns {
            let pat = raw.trim();
            if pat.is_empty() {
                continue;
            }
            let pat_lower = pat.to_lowercase();
            if !pat_lower.contains('*') {
                if title_lower.contains(&pat_lower) {
                    return true;
                }
                continue;
            }
            if glob_contains(&title_lower, &pat_lower) {
                return true;
            }
        }
        false
    }

    pub fn filter_title(title: &str, p: &PrivacySettings) -> String {
        if Self::title_matches(title, &p.deny_title_patterns) {
            String::new()
        } else {
            title.to_string()
        }
    }
}

fn is_host_port(t: &str) -> bool {
    let Some(colon) = t.find(':') else {
        return false;
    };
    let before = &t[..colon];
    if before.is_empty() || before.contains('/') {
        return false;
    }
    let after = &t[colon + 1..];
    let port_end = after.find('/').unwrap_or(after.len());
    let port = &after[..port_end];
    !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit())
}

fn valid_port_suffix(s: &str) -> bool {
    let Some(rest) = s.strip_prefix(':') else {
        return false;
    };
    if rest.is_empty() {
        return true;
    }
    rest.parse::<u16>().is_ok()
}

fn glob_contains(text: &str, pattern: &str) -> bool {
    let mut pos = 0usize;
    for piece in pattern.split('*').filter(|s| !s.is_empty()) {
        let Some(idx) = text[pos..].find(piece) else {
            return false;
        };
        pos += idx + piece.len();
    }
    true
}

fn normalize_path(path: &str) -> String {
    let replaced = path.replace('/', "\\");
    full_path(Path::new(&replaced))
        .map(|p| {
            p.to_string_lossy()
                .trim_end_matches('\\')
                .to_string()
        })
        .unwrap_or_else(|_| replaced.trim_end_matches('\\').to_string())
}

/// Expands Windows `%VAR%` environment variables (matching `Environment.ExpandEnvironmentVariables`).
fn expand_env_vars(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let Some(rel_end) = input[i + 1..].find('%')
        {
            let name = &input[i + 1..i + 1 + rel_end];
            if !name.is_empty()
                && let Ok(val) = std::env::var(name)
            {
                out.push_str(&val);
                i += rel_end + 2;
                continue;
            }
        }
        let ch = input[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}
