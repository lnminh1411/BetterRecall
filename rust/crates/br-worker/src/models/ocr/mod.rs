// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

pub mod ocr_math;
pub mod paddle_ocr;
pub mod windows_ocr;

pub use ocr_math::{CtcResult, OcrMath, TextBox};
pub use paddle_ocr::PaddleOcr;
pub use windows_ocr::WindowsOcr;
