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

pub const USAGE: &str = "Usage: br-ui --mock [--page home|settings|player|mcp|about|help] [--hidden] \
    [--mock-query <text>] [--video <abs-path>] [--offset-ms <n>] \
    [--motion system|full|limited] [--assets <abs-path>] [--mock-capturable] [--stats] \
    [--mock-mcp-home <abs-path>] [--mock-no-shell] [--mock-days-used <n>] \
    [--mock-firstrun] [--mock-tips]";

fn next_value(flag: &str, it: &mut impl Iterator<Item = String>) -> Result<String, String> {
    match it.next() {
        None => Err(format!("{flag} requires a value")),
        Some(val) if val.starts_with("--") => {
            Err(format!("{flag} requires a value (got flag '{val}')"))
        }
        Some(val) => Ok(val),
    }
}

fn next_abs_path(flag: &str, it: &mut impl Iterator<Item = String>) -> Result<PathBuf, String> {
    let val = next_value(flag, it)?;
    let path = PathBuf::from(&val);
    if !path.is_absolute() {
        return Err(format!("{flag} requires an absolute path (got '{val}')"));
    }
    Ok(path)
}

impl LaunchArgs {
    pub fn parse_from_env() -> Self {
        match Self::try_parse(std::env::args().skip(1)) {
            Ok(args) => args,
            Err(err) => {
                eprintln!("error: {err}\n{USAGE}");
                std::process::exit(2);
            }
        }
    }

    pub fn try_parse<I: IntoIterator<Item = String>>(iter: I) -> Result<Self, String> {
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
                    let val = next_value("--page", &mut it)?;
                    args.page = match val.to_ascii_lowercase().as_str() {
                        "settings" => Page::Settings,
                        "player" => Page::Player,
                        "mcp" => Page::Mcp,
                        "about" => Page::About,
                        "help" => Page::Help,
                        _ => Page::Home,
                    };
                }
                "--mock-query" => {
                    args.mock_query = Some(next_value("--mock-query", &mut it)?);
                }
                "--mock-mcp-home" => {
                    args.mock_mcp_home = Some(next_abs_path("--mock-mcp-home", &mut it)?);
                }
                "--mock-days-used" => {
                    let val = next_value("--mock-days-used", &mut it)?;
                    args.mock_days_used = val.parse::<u32>().map_err(|_| {
                        format!("--mock-days-used expects a non-negative integer (got '{val}')")
                    })?;
                }
                "--video" => {
                    args.video = Some(next_abs_path("--video", &mut it)?);
                }
                "--offset-ms" => {
                    let val = next_value("--offset-ms", &mut it)?;
                    args.offset_ms = val.parse::<u64>().map_err(|_| {
                        format!("--offset-ms expects a non-negative integer (got '{val}')")
                    })?;
                }
                "--motion" => {
                    let val = next_value("--motion", &mut it)?;
                    args.motion = match val.to_ascii_lowercase().as_str() {
                        "full" => MotionArg::Full,
                        "limited" => MotionArg::Limited,
                        _ => MotionArg::System,
                    };
                }
                "--assets" => {
                    args.assets_dir = Some(next_abs_path("--assets", &mut it)?);
                }
                _ => {}
            }
        }
        Ok(args)
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

    fn parse_slice(items: &[&str]) -> Result<LaunchArgs, String> {
        LaunchArgs::try_parse(items.iter().map(|s| (*s).to_string()))
    }

    #[test]
    fn parses_new_pages() {
        assert_eq!(parse_slice(&["--page", "mcp"]).unwrap().page, Page::Mcp);
        assert_eq!(parse_slice(&["--page", "about"]).unwrap().page, Page::About);
        assert_eq!(parse_slice(&["--page", "help"]).unwrap().page, Page::Help);
        assert_eq!(parse_slice(&["--page", "MCP"]).unwrap().page, Page::Mcp);
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
        ])
        .expect("valid args");
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
        let args = parse_slice(&["--mock"]).expect("valid args");
        assert_eq!(args.mock_mcp_home, None);
        assert!(!args.mock_no_shell);
        assert_eq!(args.mock_days_used, 0);
        assert!(!args.mock_firstrun);
        assert!(!args.mock_tips);
    }

    #[test]
    fn rejects_flag_swallowed_as_value() {
        assert!(parse_slice(&["--mock-mcp-home", "--mock-no-shell"]).is_err());
        assert!(parse_slice(&["--video", "--stats"]).is_err());
        assert!(parse_slice(&["--assets", "--mock"]).is_err());
        assert!(parse_slice(&["--page", "--mock"]).is_err());
        assert!(parse_slice(&["--mock-query", "--hidden"]).is_err());
        assert!(parse_slice(&["--mock-days-used", "--mock"]).is_err());
        assert!(parse_slice(&["--offset-ms", "--stats"]).is_err());
        assert!(parse_slice(&["--motion", "--mock"]).is_err());
        assert!(parse_slice(&["--mock-mcp-home"]).is_err());
    }

    #[test]
    fn requires_absolute_paths_for_path_flags() {
        assert!(parse_slice(&["--mock-mcp-home", r"relative\dir"]).is_err());
        assert!(parse_slice(&["--video", "clip.mp4"]).is_err());
        assert!(parse_slice(&["--assets", r"assets\demo"]).is_err());

        let ok = parse_slice(&[
            "--mock",
            "--mock-mcp-home",
            r"K:\scratch\mcp",
            "--video",
            r"K:\scratch\clip.mp4",
            "--assets",
            r"K:\scratch\assets",
        ])
        .expect("absolute paths should succeed");
        assert_eq!(ok.mock_mcp_home, Some(PathBuf::from(r"K:\scratch\mcp")));
        assert_eq!(ok.video, Some(PathBuf::from(r"K:\scratch\clip.mp4")));
        assert_eq!(ok.assets_dir, Some(PathBuf::from(r"K:\scratch\assets")));
    }
}
