// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

pub mod agent_pipe;
pub mod errors;
pub mod handler;
pub mod worker_loop;

pub use agent_pipe::{AgentPipe, ServerVerification};
pub use errors::{WorkerError, WorkerErrors};
pub use handler::{RequestHandler, WorkerReply};
pub use worker_loop::{LoopExit, PipeIo, WorkerLoop};
