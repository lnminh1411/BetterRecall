// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! BetterRecall CLI (`betterrecall.exe`): sends one command on the public IPC pipe, handles
//! single-connection two-step purge (`--yes` or console prompt), and streams `subscribe` lines.

use std::io::Write;

use br_contracts::{Envelope, LineError};
use br_core::{CancelToken, CliPurge, CliPurgeOptions, PipeClient, PipeConnectResult};

/// Usage line written to stderr when arguments do not match `ipc <words...>`.
pub const USAGE: &str = "Usage: betterrecall ipc <words...>";

/// Line written to stdout when the agent fails the client trust check.
pub const ERR_NOT_TRUSTED: &str = "err BetterRecall not trusted";

/// Transport seam for the CLI so argument grammar, purge flow, and exit codes can be unit-tested.
pub trait CliConnection {
    fn request(&mut self, line: &str) -> Result<Option<String>, LineError>;
    fn read_line(&mut self) -> Result<Option<String>, LineError>;
}

/// Real named-pipe connection wrapping [`PipeClient`].
pub struct PipeCliConnection<'a> {
    client: PipeClient,
    ct: &'a CancelToken,
}

impl<'a> PipeCliConnection<'a> {
    pub fn new(client: PipeClient, ct: &'a CancelToken) -> Self {
        Self { client, ct }
    }
}

impl CliConnection for PipeCliConnection<'_> {
    fn request(&mut self, line: &str) -> Result<Option<String>, LineError> {
        self.client.request(line, self.ct)
    }

    fn read_line(&mut self) -> Result<Option<String>, LineError> {
        self.client.read_line(self.ct)
    }
}

/// Dependencies for [`run_cli`].
pub struct CliEnv<'a, C, ConnFn, PresFn, ReadFn, W1, W2> {
    pub connect: ConnFn,
    pub not_answering: PresFn,
    pub interactive: bool,
    pub read_answer: ReadFn,
    pub stdout: &'a mut W1,
    pub stderr: &'a mut W2,
    pub _marker: std::marker::PhantomData<C>,
}

/// Runs the CLI command grammar and returns the process exit code (0, 1, 2, or 3).
pub fn run_cli<C, ConnFn, PresFn, ReadFn, W1, W2>(
    args: &[String],
    env: CliEnv<'_, C, ConnFn, PresFn, ReadFn, W1, W2>,
) -> i32
where
    C: CliConnection,
    ConnFn: FnOnce() -> (PipeConnectResult, Option<C>),
    PresFn: FnOnce() -> (&'static str, i32),
    ReadFn: FnMut() -> Option<String>,
    W1: Write,
    W2: Write,
{
    if args.len() < 2 || !args[0].eq_ignore_ascii_case("ipc") {
        let _ = writeln!(env.stderr, "{USAGE}");
        return 1;
    }

    let mut words: Vec<String> = args[1..].to_vec();
    let purge = CliPurge::applies(&words);
    let yes = purge && CliPurge::take_yes(&mut words);
    let line = words.join(" ");

    let (result, client) = (env.connect)();
    if result == PipeConnectResult::NotRunning {
        let (msg, exit) = (env.not_answering)();
        let _ = writeln!(env.stdout, "{msg}");
        return exit;
    }
    let Some(mut client) = client else {
        let _ = writeln!(env.stdout, "{ERR_NOT_TRUSTED}");
        return 2;
    };
    if result != PipeConnectResult::Ok {
        let _ = writeln!(env.stdout, "{ERR_NOT_TRUSTED}");
        return 2;
    }

    if purge {
        return CliPurge::run(CliPurgeOptions {
            request: |l: &str| client.request(l).ok().flatten(),
            preview_line: &line,
            yes,
            interactive: env.interactive,
            read_answer: env.read_answer,
            output: env.stdout,
            prompt: env.stderr,
        });
    }

    let response = match client.request(&line) {
        Ok(Some(r)) => r,
        _ => return 1,
    };
    let _ = writeln!(env.stdout, "{response}");
    let ok = match Envelope::parse(&response) {
        Ok((ok, _, _)) => ok,
        Err(_) => return 1,
    };
    if !ok {
        if words.len() >= 2
            && words[0].eq_ignore_ascii_case("purge")
            && words[1].eq_ignore_ascii_case("confirm")
        {
            let _ = writeln!(env.stderr, "{}", CliPurge::HINT_CONFIRM);
        }
        return 1;
    }

    // subscribe turns the connection into a stream: print every line until the agent closes it or Ctrl+C.
    let trimmed = line.trim_start();
    if trimmed.len() >= 9
        && trimmed.is_char_boundary(9)
        && trimmed[..9].eq_ignore_ascii_case("subscribe")
    {
        while let Ok(Some(next)) = client.read_line() {
            let _ = writeln!(env.stdout, "{next}");
        }
    }
    0
}
