// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Named pipes, peer trust checks, command processor, and CLI/agent presence helpers.

pub mod agent_presence;
pub mod audit_throttle;
pub mod cli_purge;
pub mod command_processor;
pub mod pipe_client;
pub mod pipe_manager;
pub mod pipe_server;
pub mod pipe_stream;
pub mod process_info;

pub use agent_presence::AgentPresence;
pub use audit_throttle::AuditThrottle;
pub use cli_purge::{CliPurge, CliPurgeOptions};
pub use command_processor::{
    ClientSession, CommandDeps, CommandProcessor, Reply, StreamProducer, Subscription,
};
pub use pipe_client::{PipeClient, PipeConnectResult};
pub use pipe_manager::{ClientsChangedFn, PipeManager, PipeNameFn, StoreGetterFn};
pub use pipe_server::{
    ConnectedChangedFn, ConnectionHandlerFn, ImageAllowedFn, PipeConnection, PipeKind, PipeServer,
    RejectHandlerFn,
};
pub use pipe_stream::{PipeLineReader, PipeStream};
pub use process_info::{PeerInfo, ProcessInfo, StepClock};
