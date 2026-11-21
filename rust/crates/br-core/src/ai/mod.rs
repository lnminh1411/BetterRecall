// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Cloud AI describe and chat services, API key store, HTTP transport, and providers.

pub mod api_key_store;
pub mod chat_provider;
pub mod chat_service;
pub mod chat_share;
pub mod cloud_describer;
pub mod provider_http;
pub mod providers;

pub use api_key_store::ApiKeyStore;
pub use chat_provider::{ChatProvider, ChatRequest, ProviderTurn, ToolCall, ToolOutput, ToolSpec};
pub use chat_service::{AuditFn, ChatDeps, ChatService, ProviderFactoryFn};
pub use chat_share::{
    BasicTextRedactor, ChatShareGate, ShareGateInput, SharePlan, SharedHit, TextRedactor,
};
pub use cloud_describer::{
    CloudDescriber, DescribeError, DescribeRedactor, OutgoingImage, PassThroughRedactor,
};
pub use provider_http::{
    AiBudget, ChatFailure, ChatFailureError, HttpRequest, HttpResponse, HttpTransport,
    ParsedErrorBody, ProviderErrors, ProviderHttp, ProviderStreamError, TransportError,
    UreqTransport,
};
pub use providers::{ChatProviders, ClaudeChat, GeminiChat, OpenAiChat, SseReadError, SseReader};
