# Rust port (0.3.0-dev)

Written 2026-11-21. The plan for moving everything except the WinUI UI from C#/.NET to Rust. Working document: each milestone is marked done here as it lands.

## Decisions (user, 2026-11-21)

| # | Decision |
|---|---|
| R1 | Port every part except `BetterRecall.UI`, which stays C# and WinUI 3. `BetterRecall.Contracts` stays as well, because the UI needs it; Rust mirrors the parts of it each crate uses. |
| R2 | Clean break for data. 0.3 does not read data written by 0.2.x (store, keyframe files, key and password files, settings). The wire protocols between processes stay exactly as they are, so a C# part and a Rust part can talk while the port is in progress. |
| R3 | Tests: each crate's unit tests are ported to `cargo test`, and the C# tests that start a real process (worker end-to-end, agent tests that spawn the worker, later the pipe tests the UI depends on) are run against the Rust executables as a conformance suite. `BetterRecall.UI.Tests` stays as it is. |
| R4 | Delivery part by part on `main`. A Rust part is built next to its C# twin and replaces it only once it passes the same tests. `main` stays buildable and shippable as a mixed build throughout. |
| R5 | Stop for a user review after the Worker pilot, before Agent and Core. |
| R6 | (2026-11-22) 0.3.0 ships an offline per-user exe installer written in Rust, replacing the single-file launcher. It installs to the fixed folder `%LOCALAPPDATA%\Programs\BetterRecall` without admin rights, writes an HKCU uninstall entry, and the Run value points straight at the installed agent. `bin\current` and the old-version sweeps go away. The Stub shrinks to the MCP relay, kept outside the install folder so open MCP sessions survive an upgrade. The app installs offline; models are downloaded in one of three modes: Full (every model, about 6.6 GB), Minimal (EmbeddingGemma 2, the NSFW classifier and detector, OCR and NER, about 3.5 GB; no Whisper, Silero VAD or SigLIP 2 fallback), or Install later. The OpenVINO GenAI runtime ships with the app. Sizes from the pinned catalog `rust\crates\br-fetch\models-catalog.json` (Hugging Face commit `893eb2c`; EmbeddingGemma 2 carries both its OpenVINO and ONNX graphs). |
| R7 | (2026-11-22) The app gets a Models page: it lists what is installed (from the model manifest, with size and state), opens the models folder for a manual install, and downloads a model directly from Hugging Face after a warning that names the source, size and destination. |
| R8 | (2026-11-22) The app gets an Update page with auto-update from GitHub Releases, off by default. With it off the app makes no network request unless the user presses Check. Updates are signature-checked (R9 key): a release carries an update manifest (version, installer file name, size, SHA-256) signed with ECDSA P-256. The updater rejects a release whose manifest is unsigned, carries a bad signature, or names a version not newer than the installed one (so an old signed release cannot be replayed), and runs the downloaded installer only when its size and SHA-256 match the manifest; the installer then checks its own payload's signed integrity manifest (R9) before it replaces anything. |
| R9 | (2026-11-22) Integrity check against patching. Every installed file is listed with its SHA-256 in `integrity.json`, signed with ECDSA P-256 (`integrity.sig`); the private key stays offline with the maintainer and the public key is built into every program (agent, worker, UI, MCP relay, CLI, installer), so the check lives in many files. Every program checks the signature and every file's hash at each start; a changed, missing or unlisted file (a planted DLL) stops it at once: it logs which file, shows one message (the headless worker logs and exits with an error code), and exits before any capture. Dev builds (`.devbuild` marker) skip the check. The same key signs update manifests (R8); it replaces the earlier Ed25519 choice because .NET has P-256 built in, so the C# UI verifies without a new library. This deters patching but is not a hard barrier: whoever can write the install folder can also patch the checkers. |

## Order

1. **Worker (pilot).** The smallest part (5.3k lines), behind the private binary pipe, mostly inference. No persistent data except `cache\device.json`.
2. **Agent and Core.** The big step (28.8k lines): capture, replay, audio, the encrypted store, keys and all pipe servers. This is where the data clean break (R2) applies.
3. **Installer, MCP relay, Mcp, Cli.** The launcher is replaced by the installer (R6), the Stub shrinks to the MCP relay; Mcp and Cli are small (about 0.5k lines). Models (R7) and updates (R8) land in the same phase.
4. **UI** stays C#.

## Layout

- `rust\` holds one Cargo workspace. `rust\crates\br-contracts` holds the wire types and framing (serde, camelCase JSON as `ContractsJson` writes it). `rust\crates\br-worker` builds `BetterRecall.Worker.exe`. Later crates follow the same pattern.
- The build output goes to `CARGO_TARGET_DIR` (on the dev PC `K:\cache\br-rust\target`), never into the repo. `rust\.cargo\config.toml` caps parallel jobs at 4 so a build does not take the whole PC.
- Native runtimes are not downloaded again. `onnxruntime.dll` (DirectML build) is loaded through the `ort` crate's `load-dynamic` feature, and OpenVINO plus OpenVINO GenAI are called through hand-written C bindings, as the C# worker already does. All three DLLs come from the NuGet packages the C# build already uses.
- Contract checks: a C# test writes golden JSON for each worker request and response type into `tests\fixtures\contracts\`, and Rust tests parse and re-serialise them.

## Worker pilot milestones

| Stage | Scope | Done when |
|---|---|---|
| W1 | Workspace, `br-contracts` (worker types, `BinaryFrame`, the settings the worker reads, data paths), worker host: arguments, settings from stdin or file, logging with the same rotation, process hardening, pipe client with the agent check, request loop, idle exit, fake engine | `cargo test` passes; the worker end-to-end tests pass against the Rust exe in fake mode |
| W2 | Inference: ONNX Runtime and OpenVINO backends, device and GPU selection, DXGI adapters, image decode and resize, embedder, SentencePiece tokenizer, embedding table, vectors, NSFW classifier, detector and gate, model manifest and catalog, the real engine with bench and `device.json` | the synthetic-model, protocol and unit tests pass as cargo tests; the C# agent tests that spawn the worker pass against the Rust exe |
| W3 | OCR (PaddleOCR, Windows OCR, box math), NER (BPE tokenizer, PII model, gate), audio (Silero VAD, Whisper through GenAI) | every Worker test has a cargo or conformance equivalent; real-model tests pass where models are installed |
| W4 | Measure against the C# worker: start time, embed and OCR latency, memory, CPU at idle, install size | numbers written into this file; user review (R5) |

After the review, the C# worker source and its ported unit tests were removed; `BetterRecall.Worker.csproj` builds and ships the Rust `br-worker` exe at `worker\BetterRecall.Worker.exe`, while `EndToEndTests` and `RealModelTests` remain as C# conformance tests.

## Agent and Core milestones

Started 2026-11-21. `BetterRecall.Core` is a library hosted inside the agent process, so both move together into one exe: `rust\crates\br-core` (library) and `rust\crates\br-agent` (bin, staged as `BetterRecall.Agent.exe`). The C# Core library stays in the tree until phase 3, because the C# Cli and Mcp still reference it.

Rules for every stage:
- Threads and blocking Win32 I/O, no async runtime, as in `br-worker`.
- Natives come from the files the C# agent already ships: `avif.dll` (libavif with libaom, AVIF stays on the CPU), `jxl*.dll`, `libvpl.dll`, and the SQLite3MC library from its NuGet package. No new native downloads.
- Clean break (R2): file formats may stay as they are, and nothing reads 0.2.x data.
- Each stage ports the C# unit tests for the code it moves into cargo tests, and is merged to `main` after its audit.

| Stage | Scope | Done when |
|---|---|---|
| A0 | C# conformance suite `tests\BetterRecall.Agent.Conformance`: starts an agent exe (the C# build by default, `BR_AGENT_DIR` override) on a scratch `BETTERRECALL_HOME`, with capture, replay and audio off and the passphrase key method, and drives it over the UI, public IPC and MCP pipes through the Contracts types and the C# Cli and Mcp clients | the suite passes against the C# agent |
| A1 | `br-contracts` additions (full settings, layout, pipe names, UI, IPC and MCP message types) and `br-core` foundation: settings store, logging, crypto (blob cipher, secure key, key vault with TPM, Hello, Argon2id and DPAPI, unlock password) | ported settings and crypto tests pass |
| A2 | `br-core` store: SQLite3MC through a hand-written binding loaded with libloading, encrypted store with chats and storage, vector index, export journal and exporter, recommender | ported store and storage tests pass |
| A3 | `br-core` pipes and AI: pipe server, manager and client, process trust checks, command processor, core host, cloud chat and describe providers over HTTP, API key store | ported command, pipe, MCP and provider tests pass |
| A4 | `br-agent` host: arguments, single instance, tray and message window, hotkeys, hardware probe, governance, memory isolation, worker supervisor and client, embedding scheduler, guardrails; capture, replay and audio not yet wired | A0 passes against the Rust agent |
| A5 | Capture and keyframes: output capture, change map, dHash, monitor and GPU selection, image codec, AVIF and JPEG XL writers, keyframe pipeline, encode governor, thumbnails, redaction | ported capture tests pass; keyframes land in the store |
| A6 | Replay and audio: oneVPL AV1 and Media Foundation encoders, bitstream, fMP4 writer, packet ring, replay engine and library, cursor overlay, WASAPI, AAC, speech segmenter | ported replay and audio tests pass; a saved replay plays |
| A7 | Bench, self tests and the keyframe budget run; measure against the C# agent (start, idle CPU, capture CPU with AVIF under 1%, memory, size) | numbers written here; user review before the swap |

## Installer, models and updates milestones

Rules for every stage:
- One shared Rust download path (`br-fetch`): blocking HTTPS over the `ureq` and SChannel stack `br-core` already uses, resumable, writes to a `.part` file, checks size and SHA-256 before the final rename, and checks free space first. The installer, the Models page and the updater all use it.
- Models come from the pinned revisions in `Scripts\models\common.py` and the converted-model repo on Hugging Face; the model manifest carries each file's size and SHA-256.
- Tests never touch the real HKCU Run, uninstall or app keys, or `%LOCALAPPDATA%\Programs`: the installer takes a registry root and install root override for tests. Windows Sandbox runs only on the user's go-ahead.

| Stage | Scope | Done when |
|---|---|---|
| I1 | `br-installer` crate: payload appended to the exe with a checked trailer, verified extraction into a staging folder, swap into the install folder with rollback, closing a running agent, UI and MCP relay through their quit events, HKCU uninstall entry, Run value, Start menu shortcut, silent mode (`/S`, `/modify`, `/uninstall`) | cargo tests for extraction, swap, rollback and registry writes pass under the overrides |
| I2 | Wizard UI (Win32): licences, model mode (Full, Minimal, Install later), progress; uninstaller asks whether to keep or delete the data folder | an install, a modify and an uninstall run end to end on a scratch root |
| I3 | `br-fetch` and the installer's model step: per-component download, verification, free-space check, falls back to Install later when offline | download, resume and hash-mismatch tests pass against a local test server |
| I4 | Models page (R7): agent UI-pipe verbs `models.list`, `models.openFolder`, `models.download` (progress stream, cancel) on top of `br-fetch`; UI page with the warning dialog before a download | conformance tests for the verbs pass; the page shows installed models and a download lands verified |
| I5 | Update page (R8): `update.check`, `update.download`, `update.apply` against GitHub Releases, a setting `update.auto` (default false), the installer run in silent upgrade mode, agent restart after it | tests against a local fake release feed pass; auto-update is off on a fresh install |
| I7 | Integrity (R9), lands before I6: crate `br-integrity` (manifest format, SHA-256 check of every file, unlisted-file check, P-256 verify through the pure-Rust `p256` crate, public key from one checked-in file), called first in the agent, worker and installer (the installer checks the payload's manifest before the swap); C# check in the UI from the same key file; a `br-sign` tool (`keygen` writes the private key to a path the maintainer chooses, `sign` writes `integrity.json` and `integrity.sig`); update manifests signed by the same tool | tests with a throwaway key: a flipped byte, a removed file, an added DLL, a bad signature and a dev build each behave as R9 says; the UI check passes its own tests |
| I6 | Packaging: release script builds the installer with the app payload; `release_layout.py` updated; the agent's autostart registers its own path and the `BETTERRECALL_LAUNCHER` handling goes; Stub reduced to the relay; Mcp and Cli ported | a release build installs, upgrades and uninstalls in Windows Sandbox |

## Status

- 2026-11-21: version 0.3.0-dev, plan written. W1 next.
- 2026-11-21: W1 implemented — Cargo workspace, `br-contracts` (worker protocol types, `BinaryFrame`, worker settings/layout/paths, 11 contract fixtures), `br-worker` (host, JSONC settings, log rotation, process hardening, overlapped Win32 named-pipe client, serial loop with keepalives/idle-exit, `FakeEngine`, ported W1 tests, `BR_WORKER_DIR` conformance hook); binary target named `br-worker` (Cargo forbids `.` in crate/bin names, so stage as `BetterRecall.Worker.exe`); real inference deferred to W2/W3.
- 2026-11-21: W2 implemented — real inference backends (`OnnxBackend` via dynamic `ort` `api-24` + DirectML/QNN, hand-written `OpenVinoBackend` FFI), DXGI adapter / GPU / device / NSFW selection & cache, PIL-compatible `ImagePipeline` (`decode`/`resize`/`to_patches`), mmap `EmbeddingTable`, `Embedder`, pure-Rust SentencePiece `GemmaTokenizer` (BPE + Unigram), `NsfwClassifier`/`NsfwDetector`/`NsfwGate`, `ModelManifest`/`ModelCatalog`, `RealEngine` (embed, NSFW, bench, replanning, `device.json`), W2 cargo tests, `Scripts\stage_rust_worker.py`, and `BR_WORKER_DIR` in `BetterRecall.Agent.Tests`; OCR/NER/audio deferred to W3.
- 2026-11-21: W3 implemented — OCR (`OcrMath`, `PaddleOcr`, WinRT `WindowsOcr`), NER (`BpeTokenizer`, `PiiNer`, `NerGate`), Audio (`SileroVad`, dynamic `WhisperGenAi` FFI), full `RealEngine` wiring (OCR/NER/`hello` preload/VAD/`Whisper`, zeroed image buffers), ported W3 test suites plus end-to-end silence check in `w2_tests`, and updated `Scripts\stage_rust_worker.py` (GenAI DLLs included when built, `libSkiaSharp.dll` excluded); all C# worker behaviour reproduced (`RealModelTests` stay C#-only per R3).
- 2026-11-21: W4 measured (below). Pilot awaits the user's review (R5).
- 2026-11-21: Worker swap done — C# worker source and unit tests removed; `BetterRecall.Worker.csproj` wraps `cargo build -p br-worker` and ships `BetterRecall.Worker.exe` with the native DLLs, `Scripts\stage_rust_worker.py` removed, and `Scripts\release_layout.py` collects Rust crate notices.
- 2026-11-21: A0 implemented — C# agent conformance suite in `tests\BetterRecall.Agent.Conformance` (38 process-level tests over the UI, public IPC, CLI and MCP pipes with `BR_AGENT_DIR` override and scratch `BETTERRECALL_HOME`); passes against the C# agent, 38 of 38 in 55 s, repeatably.
- 2026-11-21: A1 implemented — `br-contracts` expanded to full `Settings` (18 sections + validation), full `Layout`/`DataPaths`/`PipeNames`, UI/IPC/MCP message types, `LineProtocol`/`LineReader`/`Envelope`, `.NET`-exact JSON formatting and shared JSONC parser (reused by `br-worker`), plus 5 new golden fixtures (`Settings`, `AgentStatus`, `SearchResponse`, `ErrorReply`, `McpToolList`); new `br-core` crate (`SettingsStore`, `Log`, `CoreJson`, `BlobCipher`, `SecureKey`, `KeyVault` with TPM/DPAPI/Argon2id, `HelloInterop`, `UnlockPassword`, `win32`) with 72 ported settings, crypto, framing, layout and log tests; cargo test (216) and clippy pass, fixtures match the C# serializer byte for byte.
- 2026-11-21: A2 implemented — hand-written dynamic `sqlite3mc.dll` binding in `br-core::sqlite` (loaded via `libloading` from exe dir, debug-only NuGet fallback), full `EncryptedStore` (schema, SQLCipher `legacy=4` raw-key open + `KeyVault` `KeyLost`/`NotOpen` handling, FTS5 secure-delete, chats, AI usage, progressive thinning, space deletion, pausable legacy month-file rebuild, scoped/all purge with fences and notices, bounded checkpoint and cancellable maintenance), `BinaryVectorIndex`, `Ids`, `ExportJournal`, pure-Rust ZIP `CaptureExporter`, and `Recommender`, with 111 ported C# store, storage, chat, purge, export, vector, and recommender tests.
- 2026-11-21: A3 implemented — `br-core` finished: overlapped Win32 named pipes (`PipeStream`, `PipeServer` with SDDL/4 & 8 instances/trust checks, `PipeManager`, `PipeClient`, `ProcessInfo`/`StepClock`, `AgentPresence`, `AuditThrottle`, `CliPurge`), full `CommandProcessor` (all UI/public/MCP verbs incl. `chat`, `describe`, `export`, `bench`, `subscribe` streams, long-running cancellation), `CoreHost`, and cloud AI (`ApiKeyStore`, `ProviderHttp` over blocking `ureq` 3 + `native-tls`/SChannel with no redirects and 90 s header timeout, `CloudDescriber`, `ChatShareGate`, `BasicTextRedactor` with the C# patterns through `fancy-regex`, `ChatService`, `SseReader`, `ClaudeChat`, `GeminiChat`, `OpenAiChat`), plus 105 ported cargo tests and `FakeHttpServer`; cargo test (433) and clippy pass.
- 2026-11-21: A4 implemented — new `br-agent` binary crate (staged as `BetterRecall.Agent.exe`): CLI arguments (`--no-autostart`, `--console`, `--exit-after`, `--install-dir`, `--unregister`, `--allow-default-home`, `--dev-*`, `--selftest-*` stubbed until A7), single-instance mutex, quit event, `agent.pid`, `AgentHost` implementing `AgentControl`/`QueryEmbedder`/`Thumbnailer`, `AgentState` (`ReadGate`/`PauseController`), `Diagnostics`, `HardwareProbe`, `HotkeyParser`, Win32 hidden `MessageWindow` (tray icon/menu, hotkeys, session/logoff), `Shell`, `StorageGuard`, `Governance`/`EmbeddingScheduler`, `MemoryIsolation`, `WorkerSupervisor`/`WorkerConnection`/`WorkerClient`/`WorkerSettingsDiff`/`KillOnCloseJob`/`VcRuntime`, `HighResTimer`/`RetryBackoff`, `GuardrailPolicy`/`GuardrailService`/`WindowInspector`/`BrowserBadgeDetector`/`PasswordFieldProbe` (UIA COM), explicit seams for A5/A6, and 107 ported cargo tests (1 ignored, it would show a window); cargo test (539) and clippy pass, and the A0 conformance suite passes against the Rust agent (debug build, 38 of 38 in 49 s; the dev profile optimises the Argon2 and AES crates).
- 2026-11-21: A5 implemented — capture, keyframes, codecs, thumbnails and redaction ported into `br-agent` (`codecs` dynamic `libloading` bindings for `avif.dll`, `jxl.dll`/`jxl_threads.dll`, and `libSkiaSharp.dll`; `capture`: `OutputCapture` over DXGI Desktop Duplication / D3D11, `OutputRotation`, `ChangeMap`, `ChangeGate`, `DHash`/`DedupFilter`, `MonitorSelection`, `GpuDevices`/`GpuDeviceCache`, `VideoScaler` over D3D11 Video Processor, `ImageCodec`/`ImageCodecDecoder`, `Av1`/`AvifWriter`, `KeyframeEncoder`, `EncodeGovernor`, `KeyframePipeline`, `CaptureService`; `thumbs`: `AgentThumbnailer` with byte-bounded `ByteLruCache` and purge generation fencing; `privacy`: `Redactor`, `RedactionMasks`, `AgentDescribeRedactor` wired into `CoreHost`; full `AgentHost` wiring with A5 seams replaced, leaving only `ReplaySubsystem`/`AudioSubsystem` seams for A6) plus 117 ported cargo tests (4 ignored as CPU-heavy); cargo test (652) and clippy pass, and the A0 conformance suite passes against the Rust agent (38 of 38, with `avif.dll`, the `jxl` DLLs and `libSkiaSharp.dll` staged beside the exe).
- 2026-11-22: A6 implemented — replay and audio ported into `br-agent` (`codecs::vpl` dynamic `libvpl.dll` binding with System32 fallback; `replay`: `Bitstream`, `EncoderWatchdog`, `PacketRing` with `VirtualAlloc`/`VirtualLock`, `ReplayLayout`, `Fmp4Writer`, `ReplaySampler`, `CanvasBlit`, `CursorOverlay`, `VideoStream`, `ReplayMux`, `ReplayLibrary`, `VideoEncoders`/`EncoderPlan`/`EncoderFactory`, `MfVideoEncoder` over Media Foundation + `ICodecAPI`, `VplAv1Encoder` over oneVPL + D3D11, `ReplayPipeline`, `ReplayEngine`; `audio`: `AudioTimeline`, `SpeechSegmenter`/`Resampler`, `AacEncoder` over MF `AACMFTEncoder`, `WasapiCapture` over WASAPI loopback/mic + process-loopback `ActivateAudioInterfaceAsync`, `LoopbackHub`/`AudioReplayEncoder`/`AudioGuardTimeline`/`TranscriptSource`/`Transcriber` with `redact` & `boundary_spans`; full `CaptureService`, `OutputCapture`, `KeyframePipeline`, `HardwareProbe` and `AgentHost` wiring with `UnavailableReplay`/`UnavailableAudio` seams removed) plus 102 ported cargo tests (`replay_tests` 50, `audio_tests` 37, and 15 capture, storage-guard, guardrail and session-end cases A5 had missed); cargo test (754, 5 ignored) and clippy pass, and the A0 conformance suite passes against the Rust agent (38 of 38, with `libvpl.dll` added beside the exe). Playing a saved replay needs real desktop capture and is checked in A7.
- 2026-11-22: A7 code implemented — full benchmark (`BenchRunner`, `BenchTarget`, `AgentBenchTarget`, `AgentHost::bench_full`), dev self-tests (`DevSelfTest`: `--selftest-codec`, `--selftest-bench`, `--selftest-replay`, `--selftest-keyframe`), keyframe CPU budget run (`KeyframeBudgetRun`: `--selftest-keyframe-budget` with `--ocr-home`, `--worker`, `--store`, `--acg`, `--acg-at`, `--no-warm`, Win32 `GetProcessTimes`/`GetThreadTimes`/EcoQoS/below-normal priority), and dev CLI timers (`--dev-replay-at`, `--dev-export`, `--dev-bench` with indented `ContractsJson` output and `request_quit`) wired into `br-agent`, plus 8 ported `BenchTests` in `tests/bench_tests.rs`; replay pipeline counters are snapshotted only when a self test asks for them. cargo test (764 with the manifest tests, 5 ignored; 765 after the end-session bound) and clippy pass, and the A0 conformance suite passes against the Rust agent (38 of 38). Measured against the C# agent (below), which found the missing exe manifest, now fixed; awaiting the user's review before the swap.
- 2026-11-22: Agent swap done — C# agent source (`src\BetterRecall.Agent\**\*.cs`, `app.manifest`) and `tests\BetterRecall.Agent.Tests` removed (ported to cargo); `BetterRecall.Agent.csproj` wraps `cargo build -p br-agent` and ships `BetterRecall.Agent.exe` with `sqlite3mc.dll` (`SQLite3MC.PCLRaw.lib`), `libSkiaSharp.dll` (`SkiaSharp.NativeAssets.Win32`), `libvpl.dll`, the codec DLLs and the tray icons; `br-agent\build.rs` embeds the icon, version information and manifest through the Windows SDK `rc.exe`; the A0 conformance suite runs against the wrapper's output by default (38 of 38); `release_layout.py` collects the notices of the crates `br-agent` links. The C# Core library stays for the CLI and MCP until phase 3.
- 2026-11-22: Phase 3 planned: offline Rust installer (R6), Models page (R7), Update page with auto-update off by default (R8); stages I1 to I6 above. Integrity check (R9) and signature-checked updates added as I7, before I6.
- 2026-11-22: I1 implemented — new `br-installer` crate (`lib` + GUI `br-installer` binary with embedded icon, version info and `asInvoker` manifest via `rc.exe`), covering `ctx`, `payload` (64-byte `BRSETUP1` trailer + bounded `SliceReader` + streaming SHA-256), `extract` (staging folder, stale-PID cleanup, path traversal rejection, injectable free-space check, `.install` marker), `swap` (retry + rollback + `.BetterRecall.old-*` sweep), `running` (quit events + pure `pick_processes_to_end` tree/session/folder filter), `registry` (HKCU Uninstall/Run/InstallPath with prefix override), `shortcut` (`IShellLinkW` + `IPersistFile`), `install`/`uninstall`/`modify`, `log`, CLI flags (`/S`, `/uninstall`, `/modify`, `/norun`, `/allowdowngrade`, `--dev-root`, `--dev-reg-prefix`), and 16 integration tests under `BR_TEST_TMP` and `Software\BetterRecallTests\`; after the audit the payload is hashed once per install, and an uninstall started from inside the install folder hands over to a copy of the setup exe in the temp folder (a running exe cannot delete itself, and a per-user process may not schedule a delete at reboot). cargo test (16) and clippy pass; the real HKCU Run, Uninstall and app keys stay untouched.
- 2026-11-22: I2 implemented — Win32 Aero Wizard (`PropertySheetW` with `PSH_WIZARD | PSH_AEROWIZARD`, six `DIALOGEX` templates in `build.rs`, `Segoe UI` 9, centralized `strings.rs`, pure unit-tested `wizard::model`, worker thread with `WM_APP` progress/cancel messages), `ModelMode` (`Full`/`Minimal`/`Later`) stored in HKCU `ModelMode`, `Progress`/`Step`/`NoProgress` with cooperative cancel (`InstallError::Cancelled`, exit code 8), allowlisted delete-data uninstall with protected-root guard (`validate_delete_data_home`), `Ctx.temp_dir` for uninstall relaunch, `/models=full|minimal|later` and `/deletedata` CLI flags, plus unit and end-to-end tests in `br-installer`. After the audit, a wizard on a dev or test root never starts the app; cargo test (9 unit, 23 integration) and clippy pass, and a silent install and uninstall of a demo installer on a scratch root worked end to end. The wizard pages still need a visual check.
- 2026-11-22: I3 implemented — new `br-fetch` library crate (`Catalog::embedded()` over `models-catalog.json` with strict revision/base-URL/path/SHA-256/`manifest.json` validation, blocking `ureq` 3 + SChannel `Downloader` with `.part` resume, 1 MiB streaming SHA-256 constant-time verify, 2 s/4 s/8 s retries, HTTPS-only manual redirects up to 5 with loopback HTTP test seam, `Offline` detection, `install_components` writing `manifest.json` last with +5% free-space check, `remove_components`, `installed_state`) wired into `br-installer` (`Step::DownloadingModels`, `ModelsResult`, `InstallError::Models` exit code 9, agent start moved after model download, modify full→minimal removal, catalog-backed wizard sizes/Ready models folder/Progress bytes + MB/s + download cancel, and local HTTP server tests in `br-fetch` and `br-installer`). After the audit: the response body is read on its own thread, so Cancel answers within a quarter second and 60 s without data counts as a failed transfer that resumes from the part file; modify closes the app before removing models it may hold open and starts it again afterwards (also after a completed download, so the worker loads the new models). cargo test (br-fetch 11, installer 37) and clippy pass; no test touches the internet.

## W4: Rust worker against the C# worker

Measured 2026-11-21 on the dev PC (Intel NPU, OpenVINO 2026.4.1) with `FakeAgent --measure`. The run sends hello, five text embeds, three 1280x720 image embeds and one OCR read, then idles for 30 s. Settings: `inference.device` npu, the installed models, and memory isolation, EcoQoS and background mode as shipped. Both workers shared one data folder, so the NPU compile cache was warm. The C# worker is the `dev_build.py --only Worker` Release publish. Figures are the range over two warm runs each.

| | C# worker | Rust worker |
|---|---|---|
| Pipe connect after spawn | 131-136 ms | 49-51 ms |
| Hello (models loaded) | 15.8-17.9 s | 14.9-15.3 s |
| First text embed | 381-581 ms | 191-285 ms |
| Text embed, steady | 70-79 ms | 69-80 ms |
| First image embed | 1.45-1.67 s | 1.60-1.64 s |
| Image embed, steady | 544-551 ms | 532-544 ms |
| OCR request, models not installed | 6 ms | 2 ms |
| Private memory after the run | 459 MB | 428-432 MB |
| CPU time for the whole run | 17.1-18.6 s | 15.0-16.3 s |
| CPU over 30 s idle | 16-31 ms (0.003-0.005%) | 0 ms |
| Worker folder | 357 MB | 242 MB |
| Own code (exe, managed and runtime files) | 122.5 MB (.NET runtime, SkiaSharp, WinRT projection) | 2.9 MB exe |

Steady-state inference runs on the NPU, so it is the same in both workers. The Rust worker wins on start-up, the first request, memory, CPU and size.

Not measured:
- Speech: neither build has the OpenVINO GenAI DLLs (`artifacts\native\openvino-genai\` is absent).
- OCR and NER: no models are installed on this PC.

The first (cold) run spent minutes planning the device and compiling for the NPU. That is the same first-run cost the C# worker has. The NPU compiler logs an `UnrollPermuteDMA` error at every start in both workers.

## A7: Rust agent against the C# agent

Measured 2026-11-22 on the dev PC (20 logical processors; three outputs: 1920x1080 on the AMD GPU and two 1280x1024 on the NVIDIA GPU; replay encodes AV1 on the Intel GPU through oneVPL). Both agents are release builds with the same native DLLs and the same Rust worker: the C# agent is the `dev_build.py --only Agent,Worker` NativeAOT publish, the Rust agent `cargo build --release -p br-agent`. Each run used a fresh scratch `BETTERRECALL_HOME` with the DPAPI key method and no models installed, so the worker started but had no embedder. The user played a game during the capture and budget runs, so the screens changed all the time. The runs were sequential, so the screen content differs between agents. CPU is the share of all 20 logical processors, as Task Manager counts it.

The capture runs used default settings plus `capture.consented` true, `capture.pauseOnFullscreen` false (so keyframes ran during the fullscreen game) and `storage.minFreeGb` 2 (the scratch volume had less than the default 25 GB free). Keyframes, replay (AV1, 30 fps, 6000 kbps) and system audio were all on.

| | C# agent | Rust agent |
|---|---|---|
| Start to first pipe | 616-943 ms (3 runs) | 727-865 ms (5 runs) |
| CPU over 120 s idle, capture off | 0.023 % | 0.019 % |
| Private memory, capture off | 16 MB | 7 MB |
| CPU over 300 s, capture on | 1.015 % | 0.875 % |
| Private memory, capture on | 529 MB | 548 MB |
| Keyframe budget run, 5 min: encode CPU average | 0.431 % | 0.464 % |
| Keyframe budget run: 60 s peak (cap 1.0 %) | 0.493 % | 0.497 % |
| Keyframe budget run: 10 s peak (cap 1.5 %) | 0.734 % | 0.766 % |
| Keyframe budget run: frames encoded | 157 | 198 |
| AVIF CPU per frame, Normal / Fast level | 200 / 71 ms | 195 / 62 ms |
| Keyframe budget run: whole process CPU | 1.125 % | 1.322 % |
| `--selftest-codec` (1280x720, AVIF, WebP, JPEG XL, JPEG) | 47, 64, 71, 164 KiB | same sizes; encode times within 10 ms |
| Agent exe | 16.2 MB | 8.3 MB |
| Agent folder without symbols (243 MB of it is the shared worker) | 289 MB | 282 MB |

AVIF keyframes stay on the CPU and inside the budget in both agents: the 60 s peak is under 0.5 % against the 1 % cap. The Rust agent encoded more frames in its budget run because its screen changed more; per frame it costs the same or less. Memory with capture on is mostly the locked replay ring (95 MiB) and the D3D11 devices in both agents.

A saved replay plays: `--dev-replay-at 60 --dev-export` (with `security.requireUnlock` false, as an export needs an unlocked store) saved an encrypted 56.7 s clip of 29.9 MB, exported it, and ffmpeg decoded the whole file (AV1 4480x1140 and AAC) without errors.

Fixed during the measurements: the Rust exe embedded no manifest, so `DuplicateOutput1` failed on every output and nothing was captured (logged only at debug level, as in C#). `br-agent\build.rs` now embeds `app.manifest`, a copy of the C# agent's (Windows 10 `supportedOS`, PerMonitorV2 DPI awareness, long paths, common controls 6), and `manifest_tests` checks the exe and the copy.

Not measured:
- The full GPU bench (`--dev-bench`): it loads every encoder at several rates and was skipped while the user was playing.
- OCR, NER, embedding and speech: no models are installed on this PC.
