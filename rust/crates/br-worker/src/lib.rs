// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! BetterRecall worker process (stage W1: host, pipe protocol, and fake engine).

pub mod args;
pub mod engine;
pub mod hosting;
pub mod protocol;
pub mod vectors;

use std::path::PathBuf;
use std::time::Duration;

use br_contracts::DataPaths;

use crate::args::WorkerArgs;
use crate::engine::EngineFactory;
use crate::hosting::{Log, ProcessHardening, WorkerSettings};
use crate::protocol::{AgentPipe, LoopExit, RequestHandler, WorkerLoop};

/// Entry point for `BetterRecall.Worker.exe`.
///
/// Exit codes:
/// - `0`: normal exit (shutdown, idle, or clean pipe close)
/// - `2`: bad arguments or settings
/// - `3`: pipe connect failed
/// - `4`: pipe server is not the expected agent
/// - `5`: protocol error
pub fn run() -> i32 {
    let raw_args: Vec<String> = std::env::args().collect();
    let Some(parsed) = WorkerArgs::parse(&raw_args[1..]) else {
        return 2;
    };
    let home = parsed.home.clone();

    // Logging first, so a settings.json parse warning is written; then again at the configured level.
    Log::init(Some(DataPaths::logs_dir(&home)), "info");
    let settings = if parsed.settings_from_stdin {
        let mut stdin = std::io::stdin().lock();
        let from_agent = match WorkerSettings::read_agent_input(&mut stdin) {
            Ok(s) => s,
            Err(why) => {
                Log::error(&format!("settings from the agent: {why}; exiting"));
                return 2;
            }
        };
        match WorkerSettings::try_from_json(&from_agent, &home) {
            Ok(s) => s,
            Err(why) => {
                Log::error(&format!("settings from the agent unreadable ({why}); exiting"));
                return 2;
            }
        }
    } else {
        WorkerSettings::load(&home)
    };

    Log::init(Some(DataPaths::logs_dir(&home)), &settings.log_level);
    let settings_src = if parsed.settings_from_stdin {
        "agent"
    } else {
        "file"
    };
    let fake_pascal = if settings.fake { "True" } else { "False" };
    Log::info(&format!(
        "start settings={settings_src} modelDir={} fake={fake_pascal} device={} gpuAdapter={} dim={} tokens={} nsfwDevice={}",
        settings.model_dir.display(),
        settings.device,
        settings.gpu_adapter,
        settings.dim,
        settings.image_tokens,
        settings.nsfw_device,
    ));

    ProcessHardening::apply(settings.memory_isolation, settings.eco_qos);

    let pipe = match AgentPipe::connect(&parsed.pipe, Duration::from_secs(10)) {
        Ok(p) => p,
        Err(e) => {
            Log::error(&format!("pipe connect failed: {e}"));
            return 3;
        }
    };

    let base_dir = exe_dir();
    let expected = AgentPipe::expected_agent_image(&base_dir);
    let trusted = match pipe.verify_server(&expected) {
        Ok(res) => {
            if !res.trusted {
                Log::error(&format!("pipe server pid {} is not the agent", res.pid));
            }
            res.trusted
        }
        Err(e) => {
            Log::error(&format!("server check failed: {e}"));
            false
        }
    };
    if !trusted {
        return 4;
    }

    let engine = EngineFactory::create(&settings, &home);
    let settings_ref = settings.clone();
    let handler = RequestHandler::new(engine.as_ref(), move || settings_ref.clone());
    let exit = WorkerLoop::run(
        &pipe,
        &handler,
        Duration::from_secs(settings.idle_exit_s as u64),
        WorkerLoop::KEEPALIVE_EVERY,
        WorkerLoop::KEEPALIVE_MAX,
    );
    Log::info(&format!("exit {exit:?}"));
    if exit == LoopExit::ProtocolError {
        5
    } else {
        0
    }
}

fn exe_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("."))
}
