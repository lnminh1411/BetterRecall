<!-- SPDX-License-Identifier: AGPL-3.0-only -->
<!-- Copyright (c) BetterRecall contributors -->

# BetterRecall GPUI-CE 0.3.0 Spike (`br-ui`)

Binary: `BetterRecall.UI.Gpui.exe` (`#![windows_subsystem = "windows"]`, PerMonitorV2 DPI manifest via `gpui-ce`'s default `windows-manifest` feature, JPEG/PNG/WebP/GIF via `gpui-ce`'s default `image-default-formats` feature).

## Checks & GPUI-CE APIs

| Check | Coverage in `br-ui` | APIs used |
|---|---|---|
| **Main window & Fluent chrome** | 1400x900 default, 640x480 min, transparent custom title bar, Mica backdrop, opens on cursor's monitor, left nav (`Home`, `Settings`, `Player`). | `TitleBar::window_options()`, `TitleBar::new()`, `WindowBackgroundAppearance::MicaBackdrop`, `WindowOptions.window_min_size`, `WindowOptions.display_id` matched via Win32 `GetCursorPos` + `MonitorFromPoint` + `GetMonitorInfoW`. |
| **Warm hidden launcher & Win32 HWND control** | Created at start and kept warm (`show: false` when `--hidden` or no `--mock-query`), borderless resizable ~1200x720 centered on cursor monitor, hidden from Alt-Tab, `HWND_TOPMOST` re-asserted on every show, hides on deactivate (`observe_window_activation`) or Escape, 3 `Select` filters + 9 demo PNG cards (`assets\demo\*.png`). | `WindowOptions { show, titlebar: None, window_decorations: Some(WindowDecorations::Client), is_resizable: true }`, `Window::set_visible`, `Window::activate_window`, `Context::observe_window_activation`, `raw_window_handle::HasWindowHandle` (`gpui-ce-0.3.0/src/window.rs:9600`) -> `GetWindowLongPtrW`/`SetWindowLongPtrW` (`WS_EX_TOOLWINDOW`, clear `WS_EX_APPWINDOW`), `SetWindowPos(HWND_TOPMOST)`. |
| **Single instance & named events** | Named mutex `Local\BetterRecall.UI.Gpui.mock`; second process signals `Local\BetterRecall.UI.ShowLauncher.mock` and exits 0. First process waits on `ShowLauncher` (auto-reset) and `Quit` (`Local\BetterRecall.UI.Quit.mock`, manual-reset), both created with a current-user-only DACL matching `LauncherSignal.cs`. `ShowLauncher` shows launcher and sets query `"invoice"`; `Quit` exits cleanly. | Win32 `CreateMutexW`, `OpenProcessToken` + `GetTokenInformation(TokenUser)` + `SetEntriesInAclW` + `CreateEventW`, background `WaitForMultipleObjects` thread -> `async_channel` -> `cx.spawn` -> `cx.update_window` / `cx.quit()`. |
| **Display affinity** | `SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE)` applied to both windows when shown (never while hidden), skipped when `--mock-capturable` is passed. | `HasWindowHandle` -> `SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE)`. |
| **Theme & live OS switch** | Reads Windows light/dark, accent color, high contrast, and system UI font via `native-theme` 0.7.2 (`windows` + `watch` features); maps `ResolvedTheme` onto `gpui_component::Theme` (4px control radius, 8px card radius, Segoe UI Variable fallback). Live restyling via `native_theme::watch::on_theme_change`. | `SystemTheme::from_system()`, `SystemTheme::pick()`, `native_theme::detect::invalidate_caches()`, `native_theme::watch::on_theme_change()`, `Theme::change()` + `Theme::update()`. |
| **Limited Motion** | `MotionMode` (`system`, `full`, `limited`) global; `system` reads `SPI_GETCLIENTAREAANIMATION` and refreshes on OS theme/setting change. Live `Select` on Settings page switches `App::set_reduce_motion`. `Spinner` and indeterminate `Progress` render static when reduced. | `SystemParametersInfoW(SPI_GETCLIENTAREAANIMATION)`, `App::set_reduce_motion(bool)` (`gpui-ce-0.3.0/src/app.rs:1517`), `App::reduce_motion()` (`gpui_ce_components-0.3.0/src/spinner.rs:91`, `progress.rs:90`). |
| **Settings demo & keyboard navigation** | WinUI-style cards with `Switch`, `Slider` (with ticks + arrow keys), `Select`, searchable `Combobox`, `NumberInput`, masked password `Input` (`.mask_toggle()`), multiline `Textarea`, `DatePicker`, `TimeField` (24h), `Accordion`, `Progress` (determinate + indeterminate), `Spinner`, and `Delete…` button opening `AlertDialog` (Cancel / Delete, Escape closes). Accessible names set in code on controls. | `gpui_component::Root` (binds `tab`/`shift-tab` via `gpui_ce_components_base-0.3.0/src/root.rs:14-23`), `WindowExt::open_alert_dialog`, `Switch`, `Slider`, `Select`, `Combobox`, `NumberInput`, `Input`, `Textarea`, `DatePicker`, `TimeField`, `Accordion`, `Progress`, `Spinner`. |
| **Clipboard exclusion** | Launcher "Copy text" button writes `CF_UNICODETEXT` plus registered formats `ExcludeClipboardContentFromMonitorProcessing`, `CanIncludeInClipboardHistory` (DWORD 0), `CanUploadToCloudClipboard` (DWORD 0). | Win32 `OpenClipboard`, `EmptyClipboard`, `RegisterClipboardFormatW`, `GlobalAlloc`/`GlobalLock`, `SetClipboardData`, `CloseClipboard`. |
| **Replay player (MF + WASAPI)** | Dedicated Media Foundation `IMFSourceReader` thread (`MF_SOURCE_READER_ENABLE_ADVANCED_VIDEO_PROCESSING`, `MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS`, `MFVideoFormat_RGB32`, `MFAudioFormat_Float`/`PCM` matched to WASAPI mix format). Event-driven WASAPI shared-mode render (`IAudioClient`, `IAudioRenderClient`, `IAudioClock`) as master clock (monotonic fallback if no audio). Bounded 3-frame queue, late-frame drop, `RenderImage` BGRA upload, old frame released via `window.drop_image`. Play/Pause button + Space, seek `Slider` (`SetCurrentPosition`), `--offset-ms`, F2 stats overlay, `--stats` 1 Hz stderr output (`stats fps=.. shown=.. dropped=.. ws_mb=..`). Errors shown inline without panicking. | `IMFSourceReader`, `IAudioClient`/`IAudioClock`, `RenderImage::new` (`gpui-ce-0.3.0/src/assets.rs:311`), `Window::drop_image` (`gpui-ce-0.3.0/src/window.rs:6582`), `GetProcessMemoryInfo`. |

## Gaps, Workarounds & Findings in GPUI-CE 0.3.0

1. **Rust MSRV 1.95 required by `gpui-ce` 0.3.0**:
   - `gpui-ce-0.3.0/Cargo.toml:14` declares `rust-version = "1.95"`, and `gpui-ce-0.3.0/src/window.rs:6993` calls `Vec::push_mut` (stabilized in Rust 1.95, tracking issue #135974). Building with `rustc 1.94.1` fails with `E0658` even under `--ignore-rust-version`. Upgrading the toolchain to Rust 1.95+ is required before compiling or running the spike.
2. **`Slider` has no built-in keyboard focus, arrow-key stepping, or tick marks**:
   - `gpui_ce_components_base-0.3.0/src/slider.rs:415-520` handles pointer drag/click and `AccessibleAction::Increment`/`Decrement`, but has no `FocusHandle`/`tab_stop` and no `on_key_down` handler.
   - `gpui_ce_components-0.3.0/src/slider.rs:90-141` has no tick-mark API.
   - **Workaround in spike**: `settings_demo.rs` wraps `Slider` in a focusable container (`track_focus` with `tab_stop(true)`) handling `left`/`right`/`up`/`down` to step `SliderState::set_value`, plus a tick row rendered underneath.
3. **`Combobox` is searchable-list only (not free-text editable)**:
   - `gpui_ce_components-0.3.0/src/combobox.rs:110-127, 294` provides `.searchable(true)` (a filter box inside the dropdown), not a free-text editable trigger input.
   - **Workaround in spike**: uses `ComboboxState::new(...).searchable(true)`. If free-text entry is required in the full port, compose `Input` + `Popover` list.
4. **High-contrast live switch not watched by `native-theme` on Win32**:
   - `native-theme-0.7.2/src/watch/windows.rs:113-123` documents that WinRT `AccessibilitySettings::HighContrastChanged` does not fire outside UWP (`CoreWindow`). Light/dark, accent color, text scale, and animation changes fire live via `UISettings` events; high contrast is read at startup and on any other theme change. Full port can hook `WM_THEMECHANGED` on the main HWND if live high-contrast switching is needed.
5. **Media Foundation `MFVideoFormat_RGB32` alpha byte**:
   - `MFVideoFormat_RGB32` outputs `BGRX` with byte 3 often `0x00`, while GPUI `RenderImage` (`gpui-ce-0.3.0/src/assets.rs:321`) expects opaque BGRA. `platform/video.rs` forces `px[3] = 0xFF` during row copy (and flips bottom-up rows when `MF_MT_DEFAULT_STRIDE < 0`).

## Build & Check Commands

Set build env vars (PowerShell):
```powershell
$env:CARGO_TARGET_DIR = "K:\cache\br-spike\target"
$env:CARGO_INCREMENTAL = "0"
$env:CARGO_BUILD_JOBS = "4"
```

Build & lint (requires `rustc 1.95+`):
```powershell
cargo clippy -p br-ui -- -D warnings
cargo build -p br-ui
```

Run checks (exe at `K:\cache\br-spike\target\debug\BetterRecall.UI.Gpui.exe`):
```powershell
# 1. Missing --mock exits 2 with no output
BetterRecall.UI.Gpui.exe

# 2. Main window (Home, Mica, custom title bar, WDA_EXCLUDEFROMCAPTURE)
BetterRecall.UI.Gpui.exe --mock --page home

# 3. Settings demo (keyboard Tab/Shift+Tab, arrows, Limited Motion, Dialog)
BetterRecall.UI.Gpui.exe --mock --page settings --motion system

# 4. Capturable mode (for screenshot/parity checks)
BetterRecall.UI.Gpui.exe --mock --page settings --mock-capturable

# 5. Launcher with query & demo thumbnails + excluded clipboard button
BetterRecall.UI.Gpui.exe --mock --mock-query "invoice" --assets "K:\scratch\br-spike\wt\assets\demo"

# 6. Warm hidden start + single-instance redirection
BetterRecall.UI.Gpui.exe --mock --hidden
# Second invocation signals Local\BetterRecall.UI.ShowLauncher.mock and exits 0:
BetterRecall.UI.Gpui.exe --mock --mock-query "invoice"

# 7. Replay player with offset and 1 Hz stderr stats
BetterRecall.UI.Gpui.exe --mock --page player --video "C:\path\to\clip.mp4" --offset-ms 5000 --stats
```

## 4b pages

Added sidebar pages **MCP**, **About**, and **Help** (`--page mcp|about|help`), the 6-step first-start introduction flow, the dismissible Home support card, and reusable first-visit tips on Player and MCP.

New mock flags:
- `--mock-mcp-home <DIR>`: point the MCP page at `<DIR>\home` and `<DIR>\appdata` via `br-mcp-clients` (created if missing); without it, an in-memory fake backend is used so real client configs are never touched.
- `--mock-no-shell`: log `open-url <url>` and `clipboard-diagnostics <n> bytes` to stderr instead of launching the browser.
- `--mock-days-used <N>`: mock usage days for the Home "Support development" card (shown at `>= 14` until dismissed).
- `--mock-firstrun`: launch directly into the 6-step first-start flow (logs `firstrun-done {...}` to stderr on Finish/Skip and returns to Home).
- `--mock-tips`: show the first-visit tip cards on Player and MCP (dismissed via "Got it" or Escape).
