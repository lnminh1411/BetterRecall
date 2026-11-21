// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

pub mod kill_on_close_job;
pub mod vc_runtime;
pub mod worker_client;
pub mod worker_connection;
pub mod worker_settings_diff;
pub mod worker_supervisor;

pub use kill_on_close_job::KillOnCloseJob;
pub use vc_runtime::VcRuntime;
pub use worker_client::{ImageEmbedResult, QueryEmbedderImpl, TextEmbedResult, WorkerClient};
pub use worker_connection::{
    CRASHED_ELSEWHERE_MESSAGE, NOT_SENT_MESSAGE, PipeWorkerStream, STOPPED_MESSAGE,
    WorkerConnection, WorkerError, WorkerReply, WorkerStream, WriteGateState,
};
pub use worker_settings_diff::WorkerSettingsDiff;
pub use worker_supervisor::{ProcessWorkerLauncher, WorkerLauncher, WorkerSupervisor};
