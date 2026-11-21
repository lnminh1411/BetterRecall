// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

pub mod agent_host;
pub mod agent_state;
pub mod diagnostics;
pub mod hardware_probe;
pub mod hotkey_parser;
pub mod message_window;
pub mod shell;
pub mod storage_guard;

pub use agent_host::{AgentHost, AgentOptions};
pub use agent_state::{PauseController, ReadGate};
pub use diagnostics::Diagnostics;
pub use hardware_probe::{HardwareProbe, OutputDesc};
pub use hotkey_parser::{Hotkey, HotkeyParser};
pub use message_window::{MenuStateSnapshot, MessageWindow, TrayCommand};
pub use shell::{StartupRegistration, UiLauncher};
pub use storage_guard::{StorageDecision, StorageGuard, StorageStore};
