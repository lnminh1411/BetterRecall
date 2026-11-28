// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Page {
    Home,
    Settings,
    Player,
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
                "--page" => {
                    if let Some(val) = it.next() {
                        args.page = match val.to_ascii_lowercase().as_str() {
                            "settings" => Page::Settings,
                            "player" => Page::Player,
                            _ => Page::Home,
                        };
                    }
                }
                "--mock-query" => {
                    args.mock_query = it.next();
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
