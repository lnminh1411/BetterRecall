// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! CLI two-step purge helper (`CliPurge.cs`).

use std::io::Write;

use br_contracts::Envelope;
use serde_json::Value;

/// CLI purge: preview and confirm on ONE connection, since purge tokens belong to the connection
/// that made them. Asks at the console, or confirms at once with `--yes`.
pub struct CliPurge;

/// Options for [`CliPurge::run`].
pub struct CliPurgeOptions<'a, F, R, W1, W2> {
    pub request: F,
    pub preview_line: &'a str,
    pub yes: bool,
    pub interactive: bool,
    pub read_answer: R,
    pub output: &'a mut W1,
    pub prompt: &'a mut W2,
}

impl CliPurge {
    pub const HINT_CONFIRM: &'static str = "Purge tokens only work on the connection that made them. Run 'betterrecall ipc purge <scope>' and answer the prompt, or add --yes.";
    pub const HINT_YES: &'static str =
        "Not confirmed. Run it in a console to answer the prompt, or add --yes.";

    /// True for a purge preview the CLI should confirm itself (not `purge confirm`, not bare `purge`).
    pub fn applies<S: AsRef<str>>(words: &[S]) -> bool {
        words.len() >= 2
            && words[0].as_ref().eq_ignore_ascii_case("purge")
            && !words[1].as_ref().eq_ignore_ascii_case("confirm")
    }

    /// Removes `--yes` / `-y`; returns `true` when at least one was present.
    pub fn take_yes(words: &mut Vec<String>) -> bool {
        let before = words.len();
        words.retain(|w| w != "--yes" && w != "-y");
        words.len() < before
    }

    /// Sends the preview, asks (unless `yes`), sends the confirm on the same connection.
    /// Every response line goes to `output`; prompts and hints go to `prompt`.
    /// Returns the CLI exit code (`0` purged, `1` otherwise).
    pub fn run<F, R, W1, W2>(mut opts: CliPurgeOptions<'_, F, R, W1, W2>) -> i32
    where
        F: FnMut(&str) -> Option<String>,
        R: FnMut() -> Option<String>,
        W1: Write,
        W2: Write,
    {
        let Some(preview) = (opts.request)(opts.preview_line) else {
            return 1;
        };
        let _ = writeln!(opts.output, "{preview}");
        let Ok((ok, Some(Value::Object(map)), _)) = Envelope::parse(&preview) else {
            return 1;
        };
        if !ok {
            return 1;
        }
        let token = map
            .get("confirm")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty());
        let Some(token) = token else {
            return 1;
        };
        let rows = map
            .get("rows")
            .and_then(Value::as_i64)
            .map(|v| v as i32);

        if !opts.yes {
            if !opts.interactive {
                let _ = writeln!(opts.prompt, "{}", Self::HINT_YES);
                return 1;
            }
            match rows {
                Some(n) => {
                    let s = if n == 1 { "" } else { "s" };
                    let _ = write!(
                        opts.prompt,
                        "Delete {n} capture{s}? This cannot be undone. Type yes within 30 s to confirm: "
                    );
                }
                None => {
                    let _ = write!(
                        opts.prompt,
                        "Delete the matching captures? This cannot be undone. Type yes within 30 s to confirm: "
                    );
                }
            }
            let _ = opts.prompt.flush();
            let answer = (opts.read_answer)().unwrap_or_default();
            let trimmed = answer.trim();
            if !trimmed.eq_ignore_ascii_case("yes") && !trimmed.eq_ignore_ascii_case("y") {
                let _ = writeln!(opts.prompt, "Cancelled.");
                return 1;
            }
        }

        let confirm_line = format!("purge confirm {token}");
        let Some(done) = (opts.request)(&confirm_line) else {
            return 1;
        };
        let _ = writeln!(opts.output, "{done}");
        match Envelope::parse(&done) {
            Ok((true, _, _)) => 0,
            _ => 1,
        }
    }
}
