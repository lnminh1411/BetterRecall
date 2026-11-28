// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Page {
    Home,
    Settings,
    Player,
    Mcp,
    About,
    Help,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MotionArg {
    System,
    Full,
    Limited,
}

#[derive(Debug, Clone)]
pub struct LaunchArgs {
    pub mock: bool,
    pub page: Page,
    pub hidden: bool,
    pub mock_query: Option<String>,
    pub video: Option<PathBuf>,
    pub offset_ms: u64,
    pub motion: MotionArg,
    pub assets_dir: Option<PathBuf>,
    pub mock_capturable: bool,
    pub stats: bool,
    pub mock_mcp_home: Option<PathBuf>,
    pub mock_no_shell: bool,
    pub mock_days_used: u32,
    pub mock_firstrun: bool,
    pub mock_tips: bool,
}

impl Default for LaunchArgs {
    fn default() -> Self {
        Self {
            mock: false,
            page: Page::Home,
            hidden: false,
            mock_query: None,
            video: None,
            offset_ms: 0,
            motion: MotionArg::System,
            assets_dir: None,
            mock_capturable: false,
            stats: false,
            mock_mcp_home: None,
            mock_no_shell: false,
            mock_days_used: 0,
            mock_firstrun: false,
            mock_tips: false,
        }
    }
}

impl LaunchArgs {
    pub fn parse_from_env() -> Self {
        Self::parse(std::env::args().skip(1))
    }

    pub fn parse<I: IntoIterator<Item = String>>(iter: I) -> Self {
        let mut args = Self::default();
        let mut it = iter.into_iter();
        while let Some(arg) = it.next() {
            match arg.as_str() {
                "--mock" => args.mock = true,
                "--hidden" => args.hidden = true,
                "--mock-capturable" => args.mock_capturable = true,
                "--stats" => args.stats = true,
                "--mock-no-shell" => args.mock_no_shell = true,
                "--mock-firstrun" => args.mock_firstrun = true,
                "--mock-tips" => args.mock_tips = true,
                "--page" => {
                    if let Some(val) = it.next() {
                        args.page = match val.to_ascii_lowercase().as_str() {
                            "settings" => Page::Settings,
                            "player" => Page::Player,
                            "mcp" => Page::Mcp,
                            "about" => Page::About,
                            "help" => Page::Help,
                            _ => Page::Home,
                        };
                    }
                }
                "--mock-query" => {
                    args.mock_query = it.next();
                }
                "--mock-mcp-home" => {
                    args.mock_mcp_home = it.next().map(PathBuf::from);
                }
                "--mock-days-used" => {
                    if let Some(val) = it.next()
                        && let Ok(days) = val.parse::<u32>()
                    {
                        args.mock_days_used = days;
                    }
                }
                "--video" => {
                    args.video = it.next().map(PathBuf::from);
                }
                "--offset-ms" => {
                    if let Some(val) = it.next()
                        && let Ok(ms) = val.parse::<u64>()
                    {
                        args.offset_ms = ms;
                    }
                }
                "--motion" => {
                    if let Some(val) = it.next() {
                        args.motion = match val.to_ascii_lowercase().as_str() {
                            "full" => MotionArg::Full,
                            "limited" => MotionArg::Limited,
                            _ => MotionArg::System,
                        };
                    }
                }
                "--assets" => {
                    args.assets_dir = it.next().map(PathBuf::from);
                }
                _ => {}
            }
        }
        args
    }
}

/// Find `assets\demo` either from `--assets` or by walking up from the executable path.
pub fn resolve_demo_assets_dir(explicit: Option<&PathBuf>) -> Option<PathBuf> {
    if let Some(dir) = explicit
        && dir.is_dir()
    {
        return Some(dir.clone());
    }
    if let Ok(exe) = std::env::current_exe() {
        let mut cur = exe.parent();
        while let Some(dir) = cur {
            let candidate = dir.join("assets").join("demo");
            if candidate.is_dir() {
                return Some(candidate);
            }
            cur = dir.parent();
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        let mut cur = Some(cwd.as_path());
        while let Some(dir) = cur {
            let candidate = dir.join("assets").join("demo");
            if candidate.is_dir() {
                return Some(candidate);
            }
            cur = dir.parent();
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_slice(items: &[&str]) -> LaunchArgs {
        LaunchArgs::parse(items.iter().map(|s| (*s).to_string()))
    }

    #[test]
    fn parses_new_pages() {
        assert_eq!(parse_slice(&["--page", "mcp"]).page, Page::Mcp);
        assert_eq!(parse_slice(&["--page", "about"]).page, Page::About);
        assert_eq!(parse_slice(&["--page", "help"]).page, Page::Help);
        assert_eq!(parse_slice(&["--page", "MCP"]).page, Page::Mcp);
    }

    #[test]
    fn parses_new_mock_flags() {
        let args = parse_slice(&[
            "--mock",
            "--mock-mcp-home",
            r"K:\scratch\mcp-test",
            "--mock-no-shell",
            "--mock-days-used",
            "21",
            "--mock-firstrun",
            "--mock-tips",
        ]);
        assert!(args.mock);
        assert_eq!(
            args.mock_mcp_home,
            Some(PathBuf::from(r"K:\scratch\mcp-test"))
        );
        assert!(args.mock_no_shell);
        assert_eq!(args.mock_days_used, 21);
        assert!(args.mock_firstrun);
        assert!(args.mock_tips);
    }

    #[test]
    fn defaults_for_new_flags() {
        let args = parse_slice(&["--mock"]);
        assert_eq!(args.mock_mcp_home, None);
        assert!(!args.mock_no_shell);
        assert_eq!(args.mock_days_used, 0);
        assert!(!args.mock_firstrun);
        assert!(!args.mock_tips);
    }
}
