// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Chat provider registry (`ChatProviders.cs`).

pub mod claude_chat;
pub mod gemini_chat;
pub mod openai_chat;
pub mod sse_reader;

use std::sync::Arc;

use br_contracts::AiDefaults;

use crate::ai::chat_provider::ChatProvider;

pub use claude_chat::ClaudeChat;
pub use gemini_chat::GeminiChat;
pub use openai_chat::OpenAiChat;
pub use sse_reader::{SseReadError, SseReader};

pub struct ChatProviders;

impl ChatProviders {
    pub fn for_provider(provider: &str) -> Result<Arc<dyn ChatProvider>, String> {
        match provider {
            AiDefaults::GEMINI => Ok(Arc::new(GeminiChat)),
            AiDefaults::OPEN_AI => Ok(Arc::new(OpenAiChat)),
            AiDefaults::CLAUDE => Ok(Arc::new(ClaudeChat)),
            _ => Err(format!("Unknown provider: {provider}")),
        }
    }
}
