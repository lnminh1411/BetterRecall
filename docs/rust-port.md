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

## Order

1. **Worker (pilot).** The smallest part (5.3k lines), behind the private binary pipe, mostly inference. No persistent data except `cache\device.json`.
2. **Agent and Core.** The big step (28.8k lines): capture, replay, audio, the encrypted store, keys and all pipe servers. This is where the data clean break (R2) applies.
3. **Launcher, Stub, Mcp, Cli.** Small (2.1k lines).
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
