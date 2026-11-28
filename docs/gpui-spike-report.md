# GPUI-CE spike report (checkpoint 4a)

Written 2026-11-28. Branch `cp4a-spike`, crate `rust\crates\br-ui` (bin `br-ui.exe`; the install name `BetterRecall.UI.exe` comes at packaging, since cargo bin names cannot contain dots). Toolchain rustc 1.100.1 (GPUI-CE 0.3.0 needs 1.95 or newer). Evidence (screenshots, JSON, scripts): `K:\artifacts\BetterRecall-0.4\gpui-spike\`.

## Verdict

**Go, with the gaps below fixed in the port.** Every check that was run passes after four spike fixes. Not yet run: IME, high contrast and mixed-DPI monitor moves; they go to the checkpoint 5 sandbox tester, as the plan already says.

## Performance against the WinUI 3 baseline

Same script (`uiperf.py`), same PC, mock mode, release builds, user away. GPUI build: static CRT, `br-ui.exe` plus `assets\demo`.

| Measure | WinUI 3 | GPUI-CE spike | Change |
|---|---|---|---|
| Start to first window (median of 5) | 627 ms | 189 ms | 3.3x faster |
| Show launcher on a warm instance | 12.3 ms | 3.1 ms | 4x faster |
| Working set, idle on Home | 197 MB | 86 MB | -56% |
| Private bytes, idle | 125 MB | 63 MB | -50% |
| CPU, idle 30 s | 0.8% | 0.8% | same |
| Working set after a search | 222 MB | 91 MB | -59% |
| Install size of the UI folder | 176 MB | 44 MB | -75% |

Caveat: the spike has three pages and a nine-card launcher; WinUI has every page. Start time and memory will rise as pages arrive. The gap is large enough that the targets "same or better, start and working set clearly better" should hold.

Replay player memory (not in the WinUI baseline, which plays through `MediaPlayerElement`): private bytes level off at about 400 MB for 1080p and 580 MB for 1440p, with no growth over 60 s. That is high; see gap 6.

## Checks

| Check | Result | How |
|---|---|---|
| Keyboard | pass after fix | Sandbox, real key input: Tab walks nav, switch, slider, select, combobox, number box with a visible focus ring; arrow keys move the caret and slider; Space pauses the player. Before the fix nothing had focus, so keys did nothing until a click. Shift+Tab out of the number box does nothing (gap 3). Dialog Escape not exercised. |
| Rendering | pass | Dev PC GPU (release and debug). Windows Sandbox with vGPU **disabled** renders through WARP: all pages draw, 1080p30 plays at 28-30 fps shown, 1440p60 at 41. So a software path exists after all. |
| Theme | pass after fix | Live switch dark to light to dark in the sandbox restyles without a restart. Accent and fonts come from `native-theme`. Its dark neutrals gave invisible control borders and a grey pane, so the neutrals now follow WinUI 3 tokens outside high contrast. High contrast not tested. |
| Text input | not run | IME needs a language pack (a download) in the sandbox; Japanese and Vietnamese Telex move to the checkpoint 5 tester. The excluded-clipboard writer is built but was not run, to keep the host clipboard untouched. |
| Video | pass after fix (H.264); AV1 and HEVC on hardware only | Host: all six clips (AV1, HEVC, H.264; 1080p30 and 1440p60) play at full rate, 57-61 fps shown at 1440p60, 0-60 dropped frames in 12 s. Seek bar and pause work. A clean Windows (the sandbox) has no AV1 or HEVC decoder; the player shows the Media Foundation error inline. Before the fix every frame was dropped (0 shown). |
| Window behaviour | pass | Warm hidden start shows nothing; a second launch exits 0 and the launcher appears in 46 ms (release); launcher is `WS_EX_TOOLWINDOW`, not `WS_EX_APPWINDOW`, topmost; Escape hides it; the Quit event ends the process with exit 0; `WDA_EXCLUDEFROMCAPTURE` (0x11) on the main window on the host; the run without `--mock` exits 2. In the sandbox the affinity read back 0 (gap 5). |
| Fluent look | partial | Light and dark close to WinUI after the colour fix; caption buttons, Mica and the corner radius look right in the window but `PrintWindow` captures neither the caption buttons nor Mica. Text in single-line inputs sits about 3 px low and is clipped (gap 2). No high-contrast shot. |
| Performance | pass | Table above. |

## Fixes made during the spike

| Commit | Fix |
|---|---|
| `8f884c7` | Compile errors against the real GPUI-CE 0.3.0 and `windows` APIs (64, the code had been written without a compiler); bin renamed to `br-ui`. |
| `ddd1c44` | Player: the decoder read both streams at once, so audio buffering pushed video about 250 ms ahead and the 3-frame queue evicted every frame before it was due. Streams are now read separately. Frames are pulled on every vsync while playing (the 16 ms timer ran at about 32 Hz on the 15.6 ms system tick). Launcher window titled `BetterRecall`. |
| `a4c5011` | Initial keyboard focus inside `Root`; WinUI 3 neutral colours. |
| build | Release builds need `-C target-feature=+crt-static`, as the other release executables already use; without it the exe fails on a clean Windows with 0xC0000135 (`vcruntime140.dll`). |

## Gaps for the port

1. **Toolchain.** Rust 1.95 or newer is required. Done: `rust/rust-toolchain.toml` pins 1.100.1, which builds and passes clippy on the whole workspace.
2. **Input text baseline.** Single-line `Input` and `NumberInput` draw their text about 3 px low and clip it at the default height. Check the line height in our theme first, then patch or wrap the component.
3. **Shift+Tab inside inputs.** Shift+Tab does not leave a `NumberInput`; the input likely consumes it. Bind `shift-tab` in the input context to `focus_prev`.
4. **Slider and combobox.** `Slider` has no keyboard support or ticks (the spike wraps it); `Combobox` is search-only, so the one editable ComboBox becomes `Input` plus a popover list.
5. **Display affinity in the sandbox.** `SetWindowDisplayAffinity` read back 0 inside Windows Sandbox (a remote session) while it reads 0x11 on the host. The WinUI app should be checked in the same place before this counts as a GPUI problem.
6. **Player memory.** 400-580 MB private while playing, kept after end of stream. Next steps: keep frames in NV12 and convert on the GPU, or reuse one `RenderImage` buffer instead of allocating per frame; release the reader at end of stream.
7. **AV1 and HEVC on clean Windows.** No decoder without the Store extensions. Replays are recorded as AV1 on the iGPU, which also decodes them, so this only matters on PCs without hardware AV1 decode. The player should say so in plain words instead of the raw Media Foundation error.
8. **High-contrast live switch.** `native-theme` does not report it on Win32; hook `WM_THEMECHANGED` on the main window.
9. **`br-contracts` unused.** The spike does not use it yet (cargo warns); the port links it.

## Not covered by this spike

IME and Telex, high contrast, mixed-DPI moves across the three monitors, dialog keyboard handling, hide-on-deactivate, and the excluded clipboard in practice. All are on the checkpoint 5 sandbox tester's list.
