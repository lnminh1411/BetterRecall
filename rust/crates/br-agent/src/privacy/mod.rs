// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Privacy redaction for OCR text, keyframes, replay masks, and cloud describe (`Privacy\`).

pub mod describe_redactor;
pub mod redaction_masks;
pub mod redactor;

pub use describe_redactor::AgentDescribeRedactor;
pub use redaction_masks::RedactionMasks;
pub use redactor::{PixelBox, RedactionOptions, RedactionResult, Redactor};
