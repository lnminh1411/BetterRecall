// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

pub mod browser_badge;
pub mod password_probe;
pub mod policy;
pub mod service;
pub mod window_inspector;

pub use browser_badge::BrowserBadgeDetector;
pub use password_probe::PasswordFieldProbe;
pub use policy::{BadgeResult, GuardDecision, GuardReason, GuardrailPolicy, WindowInfo};
pub use service::{AudioGate, AudioVerdict, GuardrailService};
pub use window_inspector::{GuardSnapshot, MonitorTop, ScanResult, WindowInspector};
