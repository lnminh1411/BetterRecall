// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

use std::fmt::Write as _;

pub const GITHUB: &str = "https://github.com/lnminh1411/BetterRecall";
pub const MODELS: &str = "https://huggingface.co/lnminh1411/BetterRecall-models";
pub const LICENCES: &str = "https://github.com/lnminh1411/BetterRecall/blob/main/LICENSING.md";
pub const ISSUES_NEW: &str = "https://github.com/lnminh1411/BetterRecall/issues/new";
pub const USER_GUIDE: &str = "https://github.com/lnminh1411/BetterRecall/blob/main/docs/user-guide.md";
// TODO(user): real link
pub const SUPPORT: &str = "https://buymeacoffee.com/PLACEHOLDER";

/// Percent-encode a query parameter value per RFC 3986 unreserved characters.
pub fn percent_encode(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for byte in input.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            _ => {
                let _ = write!(out, "%{byte:02X}");
            }
        }
    }
    out
}

/// Build the pre-filled GitHub bug report URL containing only version and Windows build.
pub fn build_bug_report_url(version: &str, windows_build: &str) -> String {
    format!(
        "{ISSUES_NEW}?template=bug_report.yml&version={}&windows={}",
        percent_encode(version),
        percent_encode(windows_build)
    )
}

/// Pure rule deciding whether to display the dismissible "Support development" card on Home.
pub fn should_show_support_card(days_used: u32, dismissed: bool) -> bool {
    days_used >= 14 && !dismissed
}

/// Open a URL via the OS shell, or log it to stderr when `no_shell` (`--mock-no-shell`) is active.
pub fn open_url(url: &str, no_shell: bool) -> Result<(), String> {
    if no_shell {
        eprintln!("open-url {url}");
        return Ok(());
    }
    #[cfg(windows)]
    {
        crate::platform::shell::open_url(url)
    }
    #[cfg(not(windows))]
    {
        Err("open_url is only supported on Windows".to_string())
    }
}

/// Format a short, safe diagnostics block with no secrets, no paths, and no window titles.
pub fn format_diagnostics(
    version: &str,
    windows_build: &str,
    theme_mode: &str,
    motion_mode: &str,
) -> String {
    format!(
        "BetterRecall Diagnostics\n\
         Version: {version}\n\
         Windows: {windows_build}\n\
         Theme: {theme_mode}\n\
         Motion: {motion_mode}\n\
         Worker device: NPU (mock)\n\
         Capture: On\n\
         Instant replay: On\n\
         Recent errors:\n\
         - [WARN] capture: frame skipped while encoder busy\n\
         - [WARN] worker: audio queue reached high-water mark\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bug_report_url_encodes_only_version_and_windows() {
        let url = build_bug_report_url("0.4.0-dev+abc", "10.0.26100.2314 (24H2)&foo=bar");
        assert!(url.starts_with(ISSUES_NEW));
        let query = url.split_once('?').expect("query string").1;
        let params: Vec<(&str, &str)> = query
            .split('&')
            .map(|pair| pair.split_once('=').expect("k=v"))
            .collect();
        assert_eq!(params.len(), 3);
        assert_eq!(params[0], ("template", "bug_report.yml"));
        assert_eq!(params[1], ("version", "0.4.0-dev%2Babc"));
        assert_eq!(
            params[2],
            ("windows", "10.0.26100.2314%20%2824H2%29%26foo%3Dbar")
        );
    }

    #[test]
    fn support_card_rule() {
        assert!(!should_show_support_card(0, false));
        assert!(!should_show_support_card(13, false));
        assert!(should_show_support_card(14, false));
        assert!(should_show_support_card(90, false));
        assert!(!should_show_support_card(14, true));
        assert!(!should_show_support_card(90, true));
    }

    #[test]
    fn diagnostics_contains_no_paths_or_drive_letters() {
        let text = format_diagnostics("0.4.0-dev", "10.0.26100.2314", "Dark", "System");
        assert!(!text.contains('\\'), "diagnostics must not contain backslashes");
        assert!(!text.contains("C:"), "diagnostics must not contain C:");
        assert!(!text.contains("c:"), "diagnostics must not contain c:");
        assert!(text.contains("0.4.0-dev"));
        assert!(text.contains("10.0.26100.2314"));
        assert!(text.contains("NPU (mock)"));
    }
}
