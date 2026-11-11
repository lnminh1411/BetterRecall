# BetterRecall v1: build spec

Written 2026-11-10. `docs\design.md` is the user's design. This file turns it into a build spec: process split, protocols, setting keys, security and performance gates. `docs\research.md` holds the evidence. Order of authority: user decisions in section 1, then `design.md`, then this file, then `research.md`.

## 1. Decisions

| Topic | Decision |
|---|---|
| Licence | Split (`LICENSING.md`): client side MIT (`BetterRecall.UI`, `BetterRecall.Contracts`, UI tests), backend AGPL-3.0-only (everything else). SPDX header in every source file. Public GitHub repository |
| Release form | Install layout below, built with `Scripts/dev_build.py`. No MSIX. A single-file release is planned; its launcher is not published yet because the launch flow is changing |
| Scope of v1 | Everything in `design.md` plus IPC and MCP: replay buffer, keyframe search, audio transcripts, timeline, security options, performance work |
| Languages | All languages the models support. Nothing hard-coded to one language |
| Screen text | v1: no OCR, a keyframe is searched through its EmbeddingGemma 2 image vector, plus window title and app name. 0.2.0-dev adds optional on-device OCR (`ocr.enabled`, default on) whose redacted text is searched too (sections 4, 6.2) |
| Audio | Silero VAD, then Whisper (multilingual, auto-detect), then the transcript is embedded with the text tower and kept as searchable text |
| Video | RAM-only AV1 ring buffer (`design.md` section 3). Reaches disk only through a replay export |
| Keyframes | One per monitor every 2 s before de-duplication. Kept encrypted on disk by default, 30 days. `keyframes.enabled` turns persistence off |
| Thumbnails | Never made at capture time. The agent makes a preview from the stored keyframe when the UI asks for it and keeps it in a RAM cache only |
| Retention | 30 days by default |
| Hardware use on the dev PC | NPU: EmbeddingGemma 2, Whisper, (SigLIP 2 fallback), NSFW gate. iGPU: AV1 encode; NSFW gate only when the NPU fails. RX 6600 and RTX 3050: untouched unless the NSFW gate falls back to a graphics card (`privacy.nsfwDevice`). CPU: VAD, NSFW gate as the last fallback, light preprocessing |
| Network | No-network guarantee: BetterRecall makes no network connections while cloud AI (sections 6.1 and 6.3, `ai.enabled`, off by default) is off. Turning it on ends the guarantee for that user until it is turned off again. Describe sends an image only when the user confirms it in a dialog; chat sends captures only after the user allows them for that chat. Every request has an audit entry |
| Security | Every protection is a setting, on by default: own windows excluded from capture (`WDA_EXCLUDEFROMCAPTURE`), Windows Hello read gate, memory isolation, NSFW gate, panic flush (section 6) |
| Phase 1 | Dev builds on the developer's PC: Core Ultra 7 265K (NPU 3720, Arc iGPU), RX 6600 + RTX 3050, Windows 11 build 29704, .NET SDK 10.0.403 |

## 2. Repository layout

| Path | Owner | What it is |
|---|---|---|
| `src/BetterRecall.Contracts` | lead | Shared records, interfaces, settings, framing, pipe names. Frozen for sub-agents: request changes in your report, do not edit |
| `src/BetterRecall.Core` | backend | Settings store, key vault, blob cipher, encrypted store, vector index, hybrid search, command processor, pipe servers, retention, audit |
| `src/BetterRecall.Cli` | backend | `betterrecall.exe ipc <words...>` |
| `src/BetterRecall.Mcp` | backend | `betterrecall-mcp.exe`, MCP over stdio, client of the MCP pipe |
| `src/BetterRecall.Agent` | agent | Always-on process: capture, replay buffer and encoder, guardrails, audio capture, scheduler, worker supervisor, thumbnailer, tray, hotkeys, panic flush |
| `src/BetterRecall.Worker` | worker | Inference: EmbeddingGemma 2, SigLIP 2 fallback, Whisper, Silero VAD, NSFW classifier and detector, device selection |
| `src/BetterRecall.UI` | ui | WinUI 3: launcher, results, home page, timeline, replay list, settings, audit, first-run consent |
| `tests/BetterRecall.<Project>.Tests` | owner of the project | xUnit tests |
| `Scripts/` | lead; worker owns `Scripts/models/` | Python build and model-prep scripts, listed in `Scripts/Scripts.md` |

Only the lead edits `BetterRecall.slnx`, `Directory.Build.props` and `.gitignore`.

Install layout (also the dev output layout):

```
<install>\BetterRecall.Agent.exe        Native AOT
<install>\betterrecall.exe              CLI, Native AOT
<install>\betterrecall-mcp.exe          MCP server, Native AOT
<install>\worker\BetterRecall.Worker.exe
<install>\ui\BetterRecall.UI.exe        WinUI 3, unpackaged, self-contained Windows App SDK
```

### 2.1 Single-file release

Not published yet: the single-file launcher and its packaging script are held back while the launch flow changes. Build the install layout with `Scripts/dev_build.py`. Models are a separate download (`Scripts/models/`).

Data dir `%LOCALAPPDATA%\BetterRecall\` (env `BETTERRECALL_HOME` overrides):

```
settings.json           plain JSON, no secrets
store.db                encrypted SQLite: captures, vectors, transcripts, audit, pending jobs, saved chats, daily cloud usage
keyframes\YYYY-MM.db    encrypted SQLite, one per month, keyframe images only
replays\*.mp4.enc       exported replay clips, AES-256-GCM
keys\datakey.bin        wrapped 256-bit data key
keys\unlock.bin         unlock password: Argon2id salt + hash and the failed-try counter, never the password (security.unlockMethod password)
keys\ai-key.bin         cloud describe API key, DPAPI CurrentUser (never in settings.json, never returned by a verb)
models\                 model files (inference.modelDir overrides)
cache\compiled\         compiled model blobs, keyed by runtime version, driver version, model hash
cache\device.json       device benchmark result
logs\                   rolling logs, never any captured content, title or query text
```

## 3. Processes and data flow

| Process | Lifetime | Holds |
|---|---|---|
| Agent | logon to logoff, single instance (`Local\BetterRecall.Agent`) | data key, open store, capture, replay ring, all pipe servers |
| Worker | started by the agent, exits after `inference.idleExitS` idle | models, NPU. No key, no store |
| UI | on demand | rendered results only. No key, no store |
| CLI, MCP | per call or per MCP session | nothing |

1. Capture (agent). Desktop Duplication per output on the dGPU that drives it. Two consumers per frame:
   - Replay path at `replay.fps` (capped at the highest captured refresh rate): convert to NV12 on the source GPU, cross-adapter copy to the iGPU, with `replay.layout` = `composited` (default) composite every captured monitor into one canvas at its virtual-desktop position (only monitors with new frames are redrawn) and encode it with one encoder; with `perMonitor` encode each monitor separately. `replay.resolution` / `replay.scalePercent` then scale each encoded video, and the encoder maximum always applies (AV1 through oneVPL, then HEVC, then H.264; 1 s closed GOP; `replay.bitrateKbps` for the whole canvas), packets into a RAM ring of `replay.seconds`. Decided 2026-11-10: composited is the default, to meet the CPU and RAM gates; perMonitor stays available. Desktop audio (WASAPI loopback) encoded to AAC into the same ring, timestamps from QPC.
   - Keyframe path every `capture.intervalMs`: guardrails (section 6), de-duplication (Desktop Duplication timeout, dirty area, dHash), downscale to `capture.maxWidth`, then with `ocr.enabled` the worker `ocr` op reads the downscaled frame (lines, boxes, per-character cuts; names and addresses too when those detectors are on), then redaction (section 6.2) paints black boxes into the frame and removes the redacted words from the text and the window title, then encode once (`keyframes.format`), `IStore.AddScreenCapture` with the redacted text in `captures.text` (FTS indexes it with the title, so on-screen words feed Text matches). The row stays hidden from search until the worker has processed it.
2. Scheduler (agent): when governance allows (section 7), `IStore.TakePendingEmbeddings`, start the worker if needed, send `embed_image` with the NSFW threshold. A dropped frame calls `IStore.DropCapture(id, "nsfw")`. Otherwise `IStore.SetEmbedding`. When `keyframes.enabled` is false the store deletes the image once the job is done.
3. Audio (agent): loopback and microphone per `audio.*`, an energy pre-gate, segments of at most `audio.maxSegmentS` cut at silence, `transcribe` to the worker. Speech is stored with `IStore.AddAudioCapture` and gets an `embed_text` document job.
4. Search: a pipe client sends `search`. Core's command processor gets the query vector from `IQueryEmbedder` (agent, `embed_text` kind `query`), then calls `IStore.Search`.
5. Preview: the UI sends `thumb <id> [width]`. Core calls `IThumbnailer` (agent), which reads the keyframe with `IStore.GetKeyframe`, resizes, encodes WebP, caches it in a RAM LRU (`keyframes.thumbCacheMb`) and returns it.
6. Instant replay: `hotkeys.replay` (or the UI `replay save` verb) saves the last `replay.seconds` (default 120) to disk, then shows a notification (`replay.notify`). The agent cuts the ring at a keyframe boundary into fragmented MP4 without re-encoding, then writes `replays\<ts>.mp4.enc` through `IBlobCipher` (or a plain MP4 in `replay.exportDir` when `replay.encryptExports` is false).
7. Replay matches (`replay.searchable`): while the replay buffer runs, every `replay.sampleSeconds` (default 5) the keyframe grab of each monitor is also kept encoded in a RAM ring (`ReplaySampler`), even when the keyframe itself is skipped (no change, paused keyframes, fullscreen). Samples pass the same guardrails and title filter as keyframes. On save, the samples inside the clip (plus, per monitor, the last earlier sample when a still screen gave none in the first interval) become capture rows of kind `replay` (`IStore.AddReplayFrame`: id prefix `r`, keyframe kept, link table `replay_frames(rid, replay_id, offset_ms)`), queued for the worker like screen rows. They show in search (Replay matches) but not in the timeline. Deleting a replay deletes its rows; panic and lock clear the ring. Redaction (batch 3) applies to these frames when it lands.

## 4. Worker protocol

Private pipe `\\.\pipe\BetterRecall.worker.<32 hex>`, new random name per launch. The agent creates it with the DACL of section 6, starts the worker with `--pipe <name> --home <data dir>`, and accepts only the process id it started. The worker exits when the pipe closes. The worker reads `settings.json` itself for `inference.*` and `index.dim`.

Frame, both directions: `u32 LE headerLen`, header (UTF-8 JSON), `u32 LE payloadLen`, payload. Header at most 65,536 bytes, payload at most 67,108,864 bytes (`BinaryFrame` in Contracts). Header types are `WorkerRequest` and `WorkerResponse` in Contracts, serialised with `ContractsJson`. The worker runs requests one at a time in arrival order (one NPU queue). Each response carries the request `id`.

| op | Request fields | Request payload | Response fields | Response payload |
|---|---|---|---|---|
| `hello` | | | `hello`: `WorkerHello` | |
| `embed_image` | `format` (`webp`, `jpeg`, `png`, `bgra8`), `width`, `height`, `stride` (bgra8 only), `nsfw` (classifier threshold, null skips the classifier), `nsfwGrid` (1 to 3, see `privacy.nsfwGrid`), `nsfwDetector` (detector threshold, null skips the detector; the agent sends it only when `privacy.nsfwFilter` and `privacy.nsfwDetector` are on). The classifier runs first; the detector only on frames the classifier passed. Each threshold must be above 0 and at most 1 | image bytes | `dim`, `ms`, `dropped` (`"nsfw"` when blocked, then no payload) | `dim` float32 LE, L2-normalised |
| `embed_text` | `text`, `kind` (`query` or `document`) | | `dim`, `ms` | float32 LE, L2-normalised |
| `transcribe` | `sampleRate` 16000, `channels` 1, `format` `f32`, `source` (`system`, `mic`), `language` (`auto` or a code) | PCM float32 LE | `transcript`: `TranscribeResult` | |
| `ocr` | `format` `bgra8`, `width`, `height`, `stride`, `entities` (true runs the NER model over the text) | image bytes | `ocr`: `OcrResult` (`lines`: text, box, confidence, `cuts` = x offset of each character edge, optional `entities` with start, length, label `person` or `address`, score; `engine`; `ms`) | |
| `bench` | | | `bench`: `BenchResult` | |
| `shutdown` | | | | |

`WorkerHello.nsfwDevice` names the device the NSFW gate loaded on (`NPU`, `iGPU`, `GPU (<adapter>)`, `CPU`), null when it is not loaded. With `privacy.nsfwFilter` on, `hello` loads the gate (and the detector when `privacy.nsfwDetector` is on and installed); `hello.missing` lists `nsfw-detector` when its model is absent. A request with `nsfwDetector` set while the detector model is missing fails with `Model not installed: nsfw-detector.` (the gate fails closed).

`kind` picks the EmbeddingGemma 2 task prompt, so queries and stored transcripts land correctly in one space. Images are always documents. Vectors come back at `index.dim` (Matryoshka truncation, then L2 renormalisation).

Errors: `Model not installed: <name>.`, `Device failed: <detail>.`, `Bad request.`, `Internal error.`. With `inference.fake` true the worker returns deterministic vectors (seeded from a hash of the input), echo transcripts and no NSFW drops, so every other part runs without model files.

Backends: `IInferenceBackend` in the worker with two implementations. OpenVINO (C API, NPU, Intel GPU, CPU) is the primary on Intel. ONNX Runtime with QNN, DirectML and CPU covers Snapdragon, AMD and any GPU.

Device choice for `inference.device` = `auto`: NPU, then CPU. A GPU is used only when no NPU exists, or when the user picks `gpu`. The iGPU is reserved for replay encode and the dGPUs for the user's own work. At first run, and after a runtime version, driver version or model hash change, the worker times each allowed device on the real model, rejects any whose output cosine against the CPU reference is below 0.999 or contains NaN, keeps the fastest, and writes `cache\device.json`. If the EmbeddingGemma 2 vision tower fails on the NPU, the worker falls back to SigLIP 2 base on the NPU and reports that in `hello.model`, and the store re-embeds when the model changes.

GPU choice for `inference.device` = `gpu`: setting `inference.gpuAdapter` (`GpuSelection` in the worker). `auto` keeps the rule above: the main backend's first GPU, i.e. the Intel GPU through OpenVINO, or DirectML's default adapter when ONNX Runtime is the main backend. Any other value is a case-insensitive part of a GPU name: first an OpenVINO GPU whose `FULL_DEVICE_NAME` contains it, then a hardware DXGI adapter whose description contains it (Basic Render Driver and software adapters excluded). An Intel adapter maps to its OpenVINO twin; any other adapter runs on ONNX Runtime DirectML with that `EnumAdapters1` index as `device_id` (`DML:<n>`), with the model files the manifest lists, as on a non-Intel PC. No match logs a warning and uses `auto`. The setting is part of the `cache\device.json` key (only when it is not `auto`, so existing caches stay valid), and the name is re-resolved at every start, so a moved DXGI index re-benchmarks. ONNX Runtime cannot read OpenVINO IR, so each embedder manifest has an `onnx` map (IR file -> float32 ONNX twin, `Scripts/models/export_onnx.py`): EmbeddingGemma 2 `text.onnx` (dynamic length) and one `vision.onnx` with dynamic patch and token axes for all three budgets; SigLIP 2 `text.onnx` and `vision.onnx`. `ModelManifest.FileFor` gives the twin when the backend is ONNX Runtime. Measured 2026-11-11 through the worker code against OpenVINO CPU (cosine >= 0.999999 everywhere): EmbeddingGemma 2 image at 70/140/280 tokens RTX 3050 267/511/1068 ms, RX 6600 220/400/713 ms, text about 40 ms on both; SigLIP 2 image 36/34 ms, text 5 ms. A manifest without the map keeps the old behaviour (DirectML load fails with a clear error, CPU fallback).

OCR (`ocr.engine` = `onnx`, model folder `ocr`): PaddleOCR, Apache-2.0. PP-OCRv6 small detector (DB, sides rounded to 32, longest side at most 1920, axis-aligned boxes) and the PP-OCRv6 small recogniser (Chinese, English, Japanese and Latin-script languages) always run; eight PP-OCRv5 mobile script recognisers (Korean, Cyrillic, Arabic, Devanagari, Thai, Greek, Tamil, Telugu) load when `ocr.languages` asks for them (`auto` = the Windows user languages plus English). A line whose main confidence is under 0.85 is read again by the chosen script recognisers, and the best wins (main gets +0.05). Device order for `ocr.device` = `auto`: NPU, then the integrated GPU (both OpenVINO), then the CPU (ONNX Runtime); a graphics card (DirectML) only when `ocr.device` is `gpu`, so OCR stays off the CPU and the user's GPUs when it can. On accelerators the recogniser sees fixed shapes: batches padded to 8 lines and widths rounded up to 256, 512, 768, 1024, 1536, 2048 or 3200 px. The NPU compiles a static copy per input shape (OpenVINO reshape) on first use, at most 48 per graph, cached in `cache\compiled\ocr-<id>`. Measured 2026-11-12 (Core Ultra 7 265K, 1280x720 synthetic screens, 12 varied layouts): CPU 362 ms per screen and 1.1 s CPU time; NPU 381 ms and 0.1 s CPU time once its shapes are compiled (first start 15 s plus up to 4 s per new shape, a cached restart 2 s); integrated GPU 565 ms and 0.8 s CPU time. Latin and Cyrillic text matched the CPU exactly on every device; the NPU (FP16) dropped the space in some Korean lines. `ocr.engine` = `windows` uses `Windows.Media.Ocr` with the first installed recogniser language from `ocr.languages`, else the user profile languages. `WorkerHello.ocrDevice` is `onnx <device>` or `windows <language>`; `hello.missing` lists `ocr` and `ner` when absent. Names and addresses: model folder `ner`, Wismut/nym-pii-multilingual-small (mmBERT-small token classifier, INT8 ONNX, MIT, 41 PII labels in BIO form) on the CPU; the worker tokenises with its `tokenizer.json` (BPE with byte fallback), reads the joined lines in windows of up to 510 tokens cut at line ends, and keeps a person or address tag when its probability is at least 0.3 (recall first). Known gap: the PaddleOCR dictionaries have no Vietnamese tone letters; FTS folds diacritics so search still finds such words.

NSFW gate models: `nsfw-classifier` (Falconsai/nsfw_image_detection, ViT-base/16, Apache-2.0; `model_int8.onnx` dynamic INT8 for the CPU, `model_fp32.onnx` for the NPU and GPUs) and `nsfw-detector` (NudeNet 320n YOLOv8n, ONNX by deepghs/nudenet_onnx @ `44956a05`, Apache-2.0, run at 640 px; blocks on exposed parts and covered breasts, genitals, buttocks and anus). Device order for `privacy.nsfwDevice` = `auto`: NPU, then the integrated GPU (OpenVINO GPU whose `DEVICE_TYPE` is integrated), then a graphics card (ONNX Runtime DirectML on the DXGI adapter from `privacy.nsfwGpuAdapter`), then the CPU (ONNX Runtime, INT8). A specific choice tries that device, then the CPU. Each candidate loads both models and runs one warm-up on a zero frame; any failure moves to the next. OpenVINO blobs go to `cache\compiled\nsfw-<id>`. The worker reads these settings at start, so a change applies when the worker next starts (after `inference.idleExitS`).

## 5. Pipes for UI, IPC and MCP

Line protocol for all three (`research.md` 7.5): UTF-8, one request per `\n`, `\r` and BOM stripped, one compact JSON line back, `{"ok":true,"data":...}` or `{"ok":false,"error":"..."}`, request line at most 4,096 bytes (`LineProtocol`, `Envelope` in Contracts).

| Pipe | Name | Exists when | Allowed client image | Verb set |
|---|---|---|---|---|
| UI | `\\.\pipe\BetterRecall.ui.<SID>` | always | `<install>\ui\BetterRecall.UI.exe` | public verbs without `ipc.allow*` limits, plus UI verbs. Response lines up to 16 MiB |
| Public | `\\.\pipe\BetterRecall.<SID>` | `ipc.enabled` | any client passing the trust checks, narrowed by `ipc.allowClients` | `research.md` 7.6 |
| MCP | `\\.\pipe\BetterRecall.mcp.<SID>` | `mcp.enabled` | `<install>\betterrecall-mcp.exe` | `ping`, `version`, `status`, `search`, `get` |

Security descriptor, trust checks, public verbs, `search` JSON form, result shape, errors, subscribe and audit follow `research.md` 7.2 to 7.9 exactly, except that the pipe names carry the SID suffix as above, and `thumb` takes an optional width: `thumb <id> [width]`, width 64 to 1280, default 320.

UI-only verbs:

| Verb | `data` | Notes |
|---|---|---|
| `unlock` or `unlock <passphrase>` | `{"unlocked":true,"until":"<ISO>"}` | Agent shows Windows Hello. In passphrase mode the passphrase is the argument. With `security.unlockMethod` password the argument is the unlock password (no Hello prompt); while the data key still waits for its passphrase, the argument is that passphrase and also opens the gate. Errors `Unlock cancelled.`, `Wrong password.`, `No unlock password is set.`, `Too many attempts. Try again in <n> s.` |
| `lock` | `null` | |
| `consent` | `null` | Sets `capture.consented`. Nothing is captured before it |
| `settings get` | `Settings` | |
| `settings set <json>` | `Settings` after merge | Partial object. Error `Invalid <section.key>.`, nothing saved on error |
| `timeline <from> <to> [limit]` | `{"items":[CaptureMeta...]}` | Newest first, default 500, max 5000 |
| `keyframe <id>` | `ThumbData` at stored size | For the detail view |
| `replay save` | `ReplayClip` | Same as the replay hotkey |
| `replay list` | `{"items":[ReplayClip...]}` | |
| `replay export <id> <path>` | `null` | Writes a decrypted MP4 to a path the user picked. The UI warns that this copy is not encrypted |
| `replay delete <id>` | `null` | |
| `panic` | `null` | Same as the panic hotkey |
| `purge all` | `{"confirm":"<token>"}`, then `purge confirm <token>` | Needs an unlock in the last 60 s. Destroys the data key and deletes every store, keyframe and replay file |
| `audit [limit]` | `{"items":[AuditEntry...]}` | |
| `bench` | `BenchResult` | Forwards to the worker |
| `facets [<from> <to>]` | `FacetsReply` `{"apps":[{"name","count"}...],"sites":[...]}` | Top 30 apps and browser sites among searchable rows in the window, most rows first; feeds the launcher's app and website filters |
| `history` | `{"items":[HistoryEntry...]}` | Saved searches, newest first, at most `privacy.searchHistoryMax`. Empty while `privacy.searchHistory` is off |
| `history add <text>` | `null` | The UI saves a search when the user commits to it (Enter, opening a result, closing the launcher on results), not on every keystroke. Ignored while history is off |
| `history delete <text>` / `history clear` | `null` | |
| `password` | `PasswordStatus` `{"set","helloAvailable","retryAfterS"}` | Unlock password state |
| `password set <json>` | `null` | `{"current":"...","new":"..."}`. `current` is needed when a password exists. The first password needs the read gate open, `security.requireUnlock` off, or no Windows Hello on the PC. New password 6 to 256 characters, not only spaces. Errors `Wrong password.`, `Password must be 6 to 256 characters.` |
| `password clear <json>` | `null` | `{"current":"..."}`. Refused while `security.unlockMethod` is password: `Password in use (config security.unlockMethod).` |
| `ai` | `AiStatus` `{"enabled","keySet","provider","model","endpoint","usage","local"}` | Effective model and endpoint (defaults filled in). `usage` = `AiUsage` `{"day","requests","tokens","requestLimit","tokenLimit"}` for today (local day); `local` is true when the endpoint is a loopback address. Never the key |
| `ai setkey <key>` / `ai clearkey` | `AiStatus` | Key: printable ASCII without spaces, at most 4,096 characters (`Invalid API key.`). Stored DPAPI-encrypted in `keys\ai-key.bin`; clear overwrites, then deletes the file |
| `describe preview <id>` | `DescribePreview` `{"token","provider","model","endpoint","mime","width","height","redacted","base64"}` | The exact image that would be sent (stored keyframe; re-encoded through the thumbnailer at width 1280 when the provider cannot take its format), after `IDescribeRedactor`. Token valid 120 s, single use, at most 4 open per connection. Errors `Cloud describe is off (config ai.enabled).`, `API key not set.`, `No image to describe.` |
| `describe send <token>` | `DescribeResult` `{"text","provider","model","tookMs"}` | Sends the previewed image (section 6.1). Long-running. Audit `describe <provider>` with the image SHA-256 and count 1 (ok) or 0 (failed). Errors `Invalid or expired token.`, `Provider unreachable.`, `Provider timed out.`, `Provider error (HTTP <n>): <message>` (key scrubbed) |
| `chat list` | `{"items":[ChatSummary...]}` | Saved chats `{"id","title","created","updated","messages"}`, newest first, at most 200 (plus unsaved ones while `ai.chatHistory` is off) |
| `chat get <id>` | `ChatDetail` `{"chat","messages","approved"}` | Messages `{"seq","ts","role","text","sources"}`; `approved` says whether this chat may send captures without asking at the current `ai.share` |
| `chat rename <json>` / `chat delete <id>` / `chat clear` | `null` | `{"id","title"}`, title trimmed to 120 characters. Delete and clear are audited (`chat delete`, `chat clear`) |
| `chat send <json>` | stream of `ChatEvent` lines | `{"chat":"<id>" or absent for a new chat,"text":"..."}`, 1 to 4,000 characters. Long-running. One `{"ok":true,"data":ChatEvent}` line per event, ending with `done`, `approval` or `error` (section 6.3) |
| `chat approve <json>` / `chat deny <token>` | stream of `ChatEvent` lines | `{"token","remove":["<capture id>"...]}`: "Allow for this chat" without the removed items, then the turn continues. Deny: the model is told the user declined and answers without captures. Token valid 10 minutes, single use |

`ChatEvent` `{"type","chatId","text","token","query","sources","items","share","code","retryAt","usage"}`, fields present by type: `start` (chatId), `search` (query), `text` (text: a piece of the answer), `warning` (code `budget`, `ratelimit`, `retry`, `truncated` or `rounds`; text), `approval` (chatId, token, share, items `ChatApprovalItem` `{"id","ts","kind","app","title","site","text","imageMime","image"}` with `text` the exact text that would be sent and `image` base64, text = a note such as "images unavailable"), `sources` (`ChatSource` `{"id","ts","kind","app","title","site","replayId","replayOffsetMs"}`), `done` (usage), `error` (code `auth`, `forbidden`, `model`, `context`, `quota`, `rate`, `overloaded`, `server`, `offline`, `timeout`, `budget`, `refused`, `bad_request`, `token`, `busy`, `not_found` or `locked`; text; retryAt when the provider said when to try again). Errors found before a turn starts (`Cloud describe is off (config ai.enabled).`, `API key not set.`, `Store is locked.`, `Invalid JSON.`) are one ordinary error line.

Argument rules: `unlock <passphrase>`, `replay export <id> <path>` (the path), `settings set <json>` and `search <text>` take the rest of the line as one argument, spaces included, no quoting. Error strings added for UI verbs: `Wrong passphrase.` (passphrase unlock failed), `Unlock required.` (`purge all` without an unlock in the last 60 s). Reply shapes are the Contracts records `ItemsReply<T>`, `UnlockReply`, `PurgeConfirmReply`, `VersionReply`. Enums serialise lowercase (`screen`, `audio`, `hybrid`, `text`, `vector`); `ThumbData` carries the image in the field `base64`. Long replies: read with `LineReader` (buffered), not `LineProtocol.ReadLineAsync`.

Showing the launcher: when the UI is already running, the agent sets the auto-reset event `PipeNames.ShowLauncherEvent` instead of starting a second UI process. The UI waits on it and shows the launcher. The agent starts the UI exe only when the event does not exist.

Hotkey grammar (agent parser and UI recorder): modifiers `Ctrl`, `Alt`, `Shift`, `Win` joined with `+`, then one key: `A`-`Z`, `0`-`9`, `F1`-`F24`, `NumPad0`-`NumPad9`, `Space`, `PageUp`, `PageDown`, `Home`, `End`, `Left`, `Up`, `Right`, `Down`, `Insert`, `Delete`, `Pause`, `PrintScreen`, or one of `` ; = , - . / ` [ \ ] ' ``. An empty string means the action has no shortcut. No two actions may share a combination (`settings set` answers `Invalid hotkeys.<action>.`). The agent re-registers every hotkey when one changes and lists the actions whose `RegisterHotKey` failed (combination held by Windows or another app) in `status` as `hotkeysFailed`; the UI Shortcuts page shows a warning on those rows and, before saving a new combination, probes it with its own `RegisterHotKey` / `UnregisterHotKey`.

`search` JSON also takes `"site":"<host>"` (normalised: lower case, no `www.`); hits carry `site`. `purge` takes two more scopes on every pipe that allows purge: `purge id <id>` (one capture, the launcher's Delete) and `purge site <host>` (the launcher's "Delete all from <site>"). A `purge last` or `purge range` also deletes saved searches typed in that window.

`search` kind filter also takes `replay`; a hit and `get` of a replay frame carry `replayId` and `replayOffsetMs`.

Data verbs (`search`, `get`, `thumb`, `keyframe`, `timeline`, `audit`, `replay list`, `replay export`, `facets`, `history`, `describe`, `chat`) answer `Store is locked.` while `security.requireUnlock` is true and the read gate is closed.

## 6. Security and privacy

Keys: a random 256-bit data key encrypts `store.db`, the keyframe files (SQLite3MultipleCiphers, SQLCipher-compatible scheme, `design.md` note 8) and replay clips (`IBlobCipher`, AES-256-GCM in 64 KiB chunks, each chunk nonce derived from the file nonce and chunk index). `security.keyProtection`:

| Mode | Wrap | Unwrap at agent start |
|---|---|---|
| `auto` | `tpm` when a TPM 2.0 exists, else `passphrase` when the user set one, else `dpapi` | per chosen mode |
| `tpm` | non-exportable RSA key, Microsoft Platform Crypto Provider | silent, so capture can write |
| `passphrase` | Argon2id (64 MiB, 3 passes) derived key | waits for `unlock <passphrase>`. Capture stays paused with reason `keylocked` until then |
| `dpapi` | DPAPI CurrentUser | silent |

Windows Hello is the read gate the agent enforces: data verbs work only for `security.unlockTimeoutMin` after `unlock`, and the gate closes on session lock. This stops disk theft and other accounts. It does not stop malware running as the same user. The settings page says so.

`security.unlockMethod` password replaces Hello at the gate, for PCs without Hello or users who prefer it. The password is a gate check only; it does not encrypt anything. `keys\unlock.bin` holds `BRU1`, the Argon2id parameters (64 MiB, 3 passes, 4 lanes, reusing the `KeyVault` Argon2id code), a 16-byte salt, the 32-byte hash, the failed-try count and the lock-out end, written atomically. Verify is constant-time. Five wrong tries are free, then each wrong try locks the gate for 30 s, doubling to at most 900 s; the count survives agent restarts and resets on success. The password is never logged, stored or audited (audit `unlock password`, count 1 or 0). The unlock panel shows a password box with a "less secure than Windows Hello" note. Set, change and remove from Settings > Security.

| Setting | Default | Effect |
|---|---|---|
| `security.excludeOwnWindows` | true | UI calls `SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE)` on every window |
| `security.requireUnlock` | true | Read gate |
| `security.unlockTimeoutMin` | 15 | 1 to 1440 |
| `security.lockOnSessionLock` | true | |
| `security.memoryIsolation` | true | Agent and worker: process DACL denying `PROCESS_VM_READ`, `PROCESS_VM_WRITE`, `PROCESS_CREATE_THREAD`, `PROCESS_DUP_HANDLE` to all but SYSTEM; mitigation policies (agent: dynamic code prohibited, it is Native AOT; worker: child processes blocked; both: image loads limited to the install dir and System32, strict handle checks); replay ring and key buffers in native memory, `VirtualLock`ed after `SetProcessWorkingSetSizeEx`; key buffers wrapped with `CryptProtectMemory` when idle; `WerAddExcludedApplication` for all BetterRecall exes. UI: the same process DACL and WER exclusion |
| `security.keyProtection` | `auto` | Table above |
| `security.unlockMethod` | `hello` | `hello` or `password`. `password` is refused until a password is set (`No unlock password is set.`) |

Guardrails (agent), checked on every foreground change and again at each keyframe:

| Setting | Default | Effect |
|---|---|---|
| `privacy.denyProcesses` | password managers, remote desktop clients, `consent.exe`, `LogonUI.exe` | skip keyframe; the replay ring masks that window's rectangle before encode |
| `privacy.denyPathPrefixes` | `[]` | same, on the foreground process image path |
| `privacy.denyTitlePatterns` | `InPrivate`, `Incognito`, `Private Browsing`, `Private browsing` | same |
| `privacy.browserBadgeDetection` | true | UI Automation looks for the private badge in Chromium toolbars; a hit masks or skips. A failed check drops the frame |
| `privacy.honorDisplayAffinity` | true | skip the keyframe while any visible window has a display affinity |
| `privacy.pauseOnLock` | true | |
| `privacy.nsfwFilter` | true | NSFW gate in the worker before embedding (classifier, then the detector when on); blocked frames are deleted and never indexed |
| `privacy.nsfwThreshold` | 0.8 | 0.01 to 0.99. Classifier score that blocks. Default 0.8 was chosen by testing; change it only if needed. Lower blocks more |
| `privacy.nsfwGrid` | 1 | 1 to 3. 1 scores the whole screen; 2 adds four quadrants and a centre tile (6 runs); 3 adds a 3×3 grid instead (10 runs). Highest score wins; stops at the first block. Tiling raised false positives on real screenshots (grid 3: 3 of 49), so 1 is the default |
| `privacy.nsfwDetector` | true | also run the NudeNet detector (exposed and covered body parts) on frames the classifier passed, so suggestive images are caught |
| `privacy.nsfwDetectorThreshold` | 0.4 | 0.01 to 0.99. Detector score that blocks. Default 0.4 was chosen by testing; change it only if needed. Lower blocks more, including suggestive images |
| `privacy.nsfwDevice` | `auto` | `auto` (NPU, then integrated GPU, then graphics card, then CPU), `npu`, `igpu`, `gpu`, `cpu`. A specific device falls back to the CPU only |
| `privacy.nsfwGpuAdapter` | `auto` | graphics card for the gate's DirectML device: part of the DXGI adapter description (case-insensitive), or `auto` = the first adapter that is not software, not the Basic Render Driver and not an Intel iGPU (Intel with under 512 MB dedicated memory) |
| `privacy.panicPurgeMinutes` | 0 (range 0 to 1440) | panic flush always zeroes RAM (replay ring, queues, thumbnail cache) and runs `wal_checkpoint(TRUNCATE)`; above 0 it also purges that many minutes of rows and keyframes |
| `privacy.recordSite` | true | for Chromium browsers, UI Automation reads the address bar (first Edit control in the toolbar) on the guardrail thread; only the host is kept (`captures.site`, no path or query), never for a private window |
| `privacy.searchHistory` | true | saved searches in `store.db` (table `search_history`, encrypted with the store); turning it off deletes them |
| `privacy.searchHistoryMax` | 100 (range 10 to 1000) | older saved searches are forgotten |
| `privacy.redactPasswords` | true | password boxes (UI Automation `IsPassword` elements of the foreground window, at most 32, cached 1 s per window) and the text after password labels; works without OCR for the boxes |
| `privacy.redactCards` | true | 13 to 19 digit numbers starting 2 to 6 that pass the Luhn check |
| `privacy.redactIds` | true | IBANs (known country code, mod 97), US SSNs, secret tokens, and numbers after ID, passport, tax, social security labels (several languages) |
| `privacy.redactBirthDates` | true | dates after birth-date labels, on the same line or the next |
| `privacy.redactEmails` | false | email addresses |
| `privacy.redactPhones` | false | 9 to 15 digit phone numbers (shaped like a phone or after a phone label; dates excluded) |
| `privacy.redactNames` | false | person names from the NER model; needs `ocr.enabled` |
| `privacy.redactAddresses` | false | street, building number, postcode and city from the NER model; needs `ocr.enabled` |

Window titles go through the deny lists before they are stored (`design.md` note 14).

### 6.1 Cloud describe (opt-in network use)

The only feature that talks to the network. Off by default (`ai.enabled`). The no-network guarantee holds only while it is off. Switching it on (Settings > AI, or a Home switch if one is added) first shows a danger dialog, Cancel the default, whose first line says the guarantee ends and names the provider; Cancel leaves it off. While it is saved on, the AI settings group and the Home status line show "No-network guarantee off: cloud describe is on." Switching it off needs no dialog and the line goes away (`NetworkGuarantee` in the UI). Flow: detail pane Describe, then `describe preview`, then a dialog with provider, model, endpoint and the image to be sent, then `describe send` only when the user clicks Send. The token from the preview is the only way to send, so no verb sends an image the user has not seen. The agent (Core `CloudDescriber`, `HttpClient`, no redirects, 90 s timeout) posts the image with a fixed prompt and returns the text; nothing is stored except the audit entry.

Redaction: the image goes through `IDescribeRedactor` before the preview. `CoreHost.StartPipes` wires the agent's keyframe redactor (section 6.2): the stored keyframe is already redacted, and the agent runs OCR and the redaction detectors on it again with the current settings before it leaves the PC; if OCR fails while a text detector is on, describe fails instead of sending an unchecked image. With OCR off or not installed the stored image goes out as is and the preview says it was not checked.

| Provider | Request | Key header | Default endpoint, model |
|---|---|---|---|
| `gemini` | `POST {endpoint}/v1beta/models/{model}:generateContent`, `contents[0].parts` = text + `inlineData` | `x-goog-api-key` | `https://generativelanguage.googleapis.com`, `gemini-3.8-flash` |
| `openai-compatible` | `POST {endpoint}/chat/completions`, user message content = `text` + `image_url` data URL | `Authorization: Bearer` (omitted when no key is set, for local servers) | `https://api.openai.com/v1`, `gpt-6-luna` |
| `claude` | `POST {endpoint}/v1/messages`, `max_tokens` 1024, content = `image` (base64 source) + `text` | `x-api-key`, `anthropic-version: 2026-10-21` | `https://api.anthropic.com`, `claude-opus-5-5` |

API shapes checked against the providers' docs on 2026-11-11. Claude and OpenAI take JPEG, PNG, GIF and WebP; other keyframe formats are re-encoded first.

### 6.2 Redaction

Runs in the agent before encrypt and store, on each kept keyframe. Input: the OCR lines of the downscaled frame (text detectors need `ocr.enabled`; password boxes do not) and the password boxes mapped into keyframe pixels. Each detector finds character spans; the per-character cuts from the OCR give a box per span (2 px padding), boxes merge, and the agent paints them solid black into the frame before encoding. The redacted characters are removed from the stored text, and the window title gets the same string detectors. Fail policy: when OCR fails for a passing reason (timeout, device error) while any text detector is on, the keyframe is dropped (`dropped unchecked` in diagnostics), never stored unchecked; when the OCR model is not installed (or the Windows OCR language is missing) keyframes are stored without text and a warning is logged every 10 minutes; with no text detector on, a failed read only means no text.

Replay: the compositor fills black rectangles before encode for the deny-list masks, the live password boxes of the foreground window (refreshed at each guardrail check) and the latest redaction boxes of each monitor (from that monitor's newest keyframe, in desktop coordinates). Those boxes stay until that monitor's next keyframe, so text that appears or moves between keyframes can show unredacted in the replay for up to one capture interval (`capture.intervalMs`, or longer while frames are de-duplicated as similar).

### 6.3 Cloud chat

Uses the cloud describe settings, key and warning (section 6.1); `chat` answers `Cloud describe is off (config ai.enabled).` while it is off. UI: the Ask toggle next to the search box swaps the results for a chat, and the Ask page in the main window keeps saved chats. Both need the read gate like the other data views. The UI never sees the key; every request goes from the agent (Core `ChatService`, `ProviderHttp`).

The model gets one tool, `search_history` (query, from, to, app, site, kind), which runs the agent's own hybrid search. At most 3 searches per model step and 5 model calls per question. `ChatShareGate` is the only code that decides what of a hit may be sent, by `ai.share`:

| `ai.share` | Sent per hit (`ai.chatMaxHits` hits per search) |
|---|---|
| `metadata` | `[S1]` reference, local time, kind, app, site, replay offset |
| `text` | plus title and up to 400 characters of OCR or transcript text, both through the text redactor |
| `textImages` (default) | plus, for up to `ai.chatMaxImages` screen hits, a 768 px thumbnail, only from an image redactor that really redacts (`IDescribeRedactor.RedactsImages`) and only when it returns the image marked redacted. Until batch 3 merges there is none, so the approval and the model are told images are unavailable and only text is sent |

An unknown share value counts as `metadata`. The text redactor (`BasicTextRedactor`) masks e-mail addresses, URL paths and queries (the host stays), labelled secrets (`password:`, `pin=`, `api key:` and similar), IBANs, runs of 8 or more digits that are not dates, and token-like strings of 24 or more letters and digits, as `[redacted]`. Over-redacting is the intended failure.

Approval: the first time a chat wants to send captures the turn stops with an `approval` event listing the exact text (and image) of each item. The user can untick items, then "Allow for this chat" (`chat approve`) or "Don't share" (`chat deny`). An allowed chat sends later searches without asking, at the same or a lower share level; a new chat, a higher share level, an agent restart or a panic asks again. Answers cite `[S1]` and end with a `sources` event; source cards open the detail view.

Requests by provider (streamed over SSE, tools in each provider's own format, checked against the providers' docs on 2026-11-12):

| Provider | Request | Tool round trip |
|---|---|---|
| `gemini` | `POST {endpoint}/v1beta/models/{model}:streamGenerateContent?alt=sse`, `systemInstruction`, `tools.functionDeclarations`, `maxOutputTokens` 4096 | `functionCall` parts (the `thoughtSignature` goes back unchanged), results as `functionResponse` parts with images as `inlineData` |
| `openai-compatible` | `POST {endpoint}/chat/completions`, `stream: true` with usage, `tools` of type function | `tool_calls` deltas joined by index, results as `role: tool` messages; images in a following user message |
| `claude` | `POST {endpoint}/v1/messages`, `stream: true`, `max_tokens` 4096, `tools` | `tool_use` blocks (`input_json_delta` joined), results as `tool_result` blocks with image blocks inside; thinking blocks go back unchanged |

Transcripts are kept per chat in the provider's own format. When the provider or model changes, the next question starts a new transcript with a short recap of the earlier messages.

Errors and quota (`ProviderErrors`): 429 that is not a quota or billing error, 5xx and 529 are retried at most twice, waiting what `Retry-After` or Gemini `RetryInfo` says (else 1 s, then 2 s) and never more than 20 s; a longer wait fails at once with the time to try again. Each retry shows a warning. OpenAI `insufficient_quota` and spend limits, Claude `billing_error`, Gemini `RESOURCE_EXHAUSTED` without a retry delay and HTTP 402 say the quota or credit is used up. 401, 403, 404, context too long, offline, a 90 s wait for headers and a 5 minute stream each get a plain message; error text from the provider is shown with the key replaced by `***`. `anthropic-ratelimit-*` and `x-ratelimit-*` headers below 10% left give a warning. Local budget: before each request the agent checks today's counts (`ai_usage` table, local day, 60 days kept) against `ai.dailyRequests` and `ai.dailyTokens`; at 80% it warns once per question, at 100% it stops before sending. A failed question leaves the chat as it was.

Audit: one entry per provider request, verb `chat <provider>`, count = captures sent with that request, SHA-256 of the transcript sent. Saved chats (tables `chats`, `chat_messages`) are encrypted with the store; `ai.chatHistory` off keeps chats in agent memory only and deletes saved ones. `purge all` and panic delete all chats; `purge last` and `purge range` delete chats with a message in that window.

## 7. Performance

Release gates for v1, measured on the dev PC with a game or a full build in the foreground.

| Metric | Target |
|---|---|
| Agent idle CPU, static screen, replay on | under 1% of one core over 1 minute |
| Agent idle CPU, replay off | under 0.5% of one core |
| Agent working set | under 120 MB plus the replay ring, with 200k vectors indexed |
| Replay ring RAM | under 100 MB for 120 s of one 1440p monitor at 15 fps |
| Keyframe tick cost | under 5 ms CPU when the screen is static |
| Worker | no process when the queue has been empty for `inference.idleExitS` |
| dGPU impact | no compute or encode work on either dGPU beyond the Desktop Duplication copy and one downscale |
| Fullscreen D3D | keyframes and inference pause (`capture.pauseOnFullscreen`); replay keeps running if `replay.duringFullscreen` |
| Search latency | under 300 ms, 200k vectors, worker warm |
| Launcher open | under 150 ms from hotkey, UI already running |

Governance (agent): EcoQoS on agent and worker, `PROCESS_MODE_BACKGROUND_BEGIN` on the worker, defer inference above `governance.deferCpuPercent` CPU or `governance.deferGpuPercent` iGPU load, pause inference on battery, drain the queue when the user is idle. `SHQueryUserNotificationState` detects fullscreen D3D and presentation mode.

Vector search needs no ANN library at v1 sizes: 30 days of 2 s keyframes after de-duplication is about 100k to 250k rows. Core keeps the binary copy in RAM (96 bytes per row at 768-d), scans it with SIMD, and reranks the top candidates with the stored vectors. Each row stores `model_id`, `revision`, `dim`, the float32 vector at its `dim`, and the binary copy. A query vector is truncated to each row group's `dim` (`design.md` section 4). `IVectorIndex` hides the scan so an HNSW sidecar can replace it later.

## 8. Settings

`settings.json`, camelCase, one object per section. `Settings` in Contracts holds every key and default. Core validates.

| Key | Default | Range or values |
|---|---|---|
| `capture.consented` | false | set by `consent` |
| `capture.enabled` | true | |
| `capture.intervalMs` | 2000 | 500 to 60000 |
| `capture.monitors` | `[]` (all) | monitor indices 0 to 63 |
| `capture.maxWidth` | 1280 | 640 to 3840 |
| `capture.dhashThreshold` | 6 | 0 to 32 |
| `capture.dirtyMinPercent` | 0.5 | 0 to 100 |
| `capture.eventTriggers` | true | foreground change, scroll stop, typing pause |
| `capture.pauseOnFullscreen` | true | |
| `keyframes.enabled` | true | persist keyframe images |
| `keyframes.format` | `webp` | `webp`, `jpeg` |
| `keyframes.quality` | 75 | 30 to 95 |
| `keyframes.maxGb` | 20 | 1 to 2000 |
| `keyframes.thumbCacheMb` | 64 | 8 to 1024 |
| `replay.enabled` | true | |
| `replay.seconds` | 120 | 15 to 600 |
| `replay.fps` | 30 | 10, 15, 24, 25, 30, 48, 50, 60, 72, 75, 90, 100, 120, 144, 165, 240. Capped at the monitor's refresh rate. Higher rates cost encoder time and ring RAM |
| `replay.bitrateKbps` | 6000 | 500 to 50000, per encoded video (the canvas, or each monitor in `perMonitor`) |
| `replay.audio` | true | desktop audio in the ring |
| `replay.encoder` | `auto` | `auto` (AV1, then HEVC, then H.264), `av1`, `hevc`, `h264`. On Intel, AV1 goes through oneVPL because the driver exposes no AV1 Media Foundation encoder; other codecs and vendors (NVENC, AMF) go through Media Foundation hardware encoders |
| `replay.encoderAdapter` | `auto` | `auto` (integrated GPU first, else the GPU driving the most captured monitors) or a substring of the adapter name |
| `replay.layout` | `composited` | `composited`: one video, all monitors on one canvas at their desktop positions. `perMonitor`: one video stream per monitor, one encoder each (more CPU and RAM, full quality per screen) |
| `replay.resolution` | `native` | `native`, `2160p`, `1440p`, `1080p`, `900p`, `720p`, `540p`, `scale`. A height target for each encoded video (canvas or monitor), aspect kept, never upscaled |
| `replay.scalePercent` | 100 | 25 to 100, used when `replay.resolution` is `scale` |
| `replay.duringFullscreen` | true | |
| `replay.encryptExports` | true | |
| `replay.exportDir` | empty (`<data>\replays`) | path, used when exports are not encrypted |
| `replay.notify` | true | Windows notification after a save; clicking it opens the UI replay list |
| `replay.searchable` | true | sample saved replays for search (section 3, step 7) |
| `replay.sampleSeconds` | 5 | 2 to 60 |
| `retention.days` | 30 | 1 to 3650 |
| `audio.system` | false | system audio transcripts |
| `audio.mic` | false | microphone transcripts |
| `audio.language` | `auto` | `auto` or a Whisper language code |
| `audio.maxSegmentS` | 30 | 5 to 30 |
| `index.dim` | 768 | 768, 512, 256 |
| `index.weakMatchBelow` | 0.65 | 0 to 1. A hit with no keyword match and a cosine below this is returned with `weak: true` and the UI labels it "Weak match". Never hidden. 0 turns labelling off. From the retrieval test (`Scripts
etrieval`): right screens 0.68-0.80, wrong or off-topic up to 0.61 |
| `privacy.*` | section 6 | |
| `ocr.enabled` | true | read text in each kept keyframe (section 3, step 1) |
| `ocr.engine` | `onnx` | `onnx` (PaddleOCR, model folder `ocr`), `windows` (`Windows.Media.Ocr`, installed OCR languages) |
| `ocr.languages` | `auto` | `auto` (Windows user languages plus English) or 1 to 16 comma-separated codes like `en`, `ja`, `zh-Hant` (at most 128 characters) |
| `ocr.device` | `auto` | `auto` (NPU, then integrated GPU, then CPU; never a graphics card), `npu`, `igpu`, `gpu`, `cpu`; `onnx` only. Read when the worker starts |
| `inference.device` | `auto` | `auto`, `npu`, `gpu`, `cpu` |
| `inference.gpuAdapter` | `auto` | GPU for `inference.device` = `gpu`: part of an OpenVINO GPU name or a DXGI adapter description (case-insensitive, 1-128 characters), or `auto` = the integrated GPU through OpenVINO when there is one, else the first graphics card (section 4) |
| `inference.modelDir` | empty (`<data>\models`) | path |
| `inference.imageTokens` | 140 | 70, 140, 280 |
| `inference.idleExitS` | 120 | 10 to 3600 |
| `inference.fake` | false | dev only |
| `governance.ecoQos` | true | |
| `governance.deferCpuPercent` | 70 | 10 to 100 |
| `governance.deferGpuPercent` | 60 | 10 to 100 |
| `governance.pauseOnBattery` | true | |
| `security.*` | section 6 | |
| `hotkeys.search` | `Ctrl+Alt+Space` | |
| `hotkeys.pause` | `Ctrl+Alt+P` | |
| `hotkeys.replay` | `Ctrl+Alt+R` | |
| `hotkeys.panic` | `Ctrl+Alt+Shift+X` | |
| `hotkeys.timeline` | empty | opens the main window on the timeline |
| `hotkeys.home` | empty | opens the main window on Home |
| `hotkeys.lock` | empty | locks the history (same as the `lock` verb) |
| `ui.theme` | `system` | `system`, `light`, `dark` |
| `ui.searchAlwaysOnTop` | true | search window topmost while shown |
| `ui.startMinimized` | true | agent start shows only the tray icon; off also opens the main window on Home |
| `ipc.*`, `mcp.enabled` | `research.md` 7.2 | |
| `ai.enabled` | false | cloud describe (section 6.1) |
| `ai.provider` | `gemini` | `gemini`, `openai-compatible`, `claude` |
| `ai.endpoint` | empty (provider default) | `https://` URL, or `http://` to a loopback host; no user info, query or fragment; at most 512 characters |
| `ai.model` | empty (provider default) | 1 to 128 of letters, digits, `. _ : / @ -`, starting with a letter or digit |
| `ai.share` | `textImages` | what a chat may send per capture: `textImages`, `text`, `metadata` (section 6.3) |
| `ai.chatMaxHits` | 8 (range 1 to 20) | captures per chat search |
| `ai.chatMaxImages` | 3 (range 0 to 8) | redacted images per chat search |
| `ai.dailyRequests` | 200 (range 0 to 100,000) | cloud requests per local day; 0 = no limit. Warning at 80%, stop at 100% |
| `ai.dailyTokens` | 500,000 (range 0 to 1,000,000,000) | cloud tokens (input + output) per local day; 0 = no limit |
| `ai.chatHistory` | true | keep chats in `store.db`; turning it off deletes saved chats |
| `log.level` | `info` | `debug`, `info`, `warn`, `error` |

## 9. Build

- .NET SDK 10.0.403, `BetterRecall.slnx`, `Directory.Build.props` (nullable on, artifacts output under `artifacts\`, `win-x64`).
- Agent, CLI and MCP publish Native AOT. Worker publishes self-contained. UI publishes unpackaged and self-contained (`WindowsPackageType=None`, `WindowsAppSDKSelfContained=true`).
- Contracts and Core are AOT and trim compatible: System.Text.Json source generation only.
- Build one project without rebuilding Contracts: `dotnet build src/<Project> -p:BuildProjectReferences=false`.
- Model files are never committed. Model download needs the user's go-ahead; until then use `inference.fake`.

## 10. Benchmark and recommendations

The UI page "Benchmark" runs a full benchmark on the user's own hardware and proposes settings. Nothing changes until the user presses Apply (all, or per row), which sends `settings set` with the chosen values.

Verbs (UI pipe only): `bench full` runs it and returns `BenchReport` (can take minutes; the UI shows progress as elapsed time and allows Cancel by closing the connection). `bench last` returns the stored report from `cache\bench.json`, or `Not found.`. `bench` keeps its old meaning (worker device bench).

Who measures what:

| Part | Measures | Where |
|---|---|---|
| Agent | `HardwareInfo` (CPU, cores, RAM, battery, GPUs with vendor, integrated flag, VRAM, LUID, driver and the hardware codecs each one exposes through oneVPL or Media Foundation, NPU, monitors with size, refresh and adapter); `EncoderBench` for each usable adapter x codec (av1, hevc, h264) x layout (composited, perMonitor) at native size and at 1080p, at 30 and 60 fps (and the highest refresh rate when above 60), 4 s each on live desktop frames; `CaptureBench` (tick, keyframe encode, cross-adapter copy) | `IAgentControl.BenchFullAsync` |
| Worker | `BenchResult` with `Tokens`: image ms for each passing device at 70, 140 and 280 image tokens, plus text ms. With more than one GPU, the `bench` op also measures every GPU outside the allowed set as its own row (OpenVINO GPUs, then each hardware DXGI adapter without an OpenVINO twin on DirectML); those rows are never chosen, their embedder is freed right after timing, GPU rows carry `Adapter` (the name) and `AdapterAuto`, and `ChosenAdapter` names the chosen GPU. Token budgets stay timed on the chosen device only. One GPU: rows as before | `bench` op |
| Core | `SearchBench` on the live index (20 random queries, vector and hybrid); then `Recommender` | `bench full` verb |

Recommendation rules (`Recommender` in Core, a pure function of `BenchReport` and current `Settings`, unit-tested with fixed reports for several hardware profiles). Each rule yields a `Recommendation` only when the value differs from the current one.

| Key | Rule | Reason code |
|---|---|---|
| `inference.device` | the device the worker chose (fastest passing the cosine check), `auto` when that equals the auto policy | `R_device_fastest` |
| `inference.gpuAdapter` | when the fastest passing row is a named GPU (more than one GPU): its name, or `auto` when it is the GPU `auto` picks; skipped when the current value is already part of that name. `inference.device` then becomes `gpu` (or stays `auto` under the auto policy) | `R_gpu_fastest` |
| `inference.imageTokens` | the largest of 280, 140, 70 whose image ms on that device keeps the embed duty under 50%: `imageMs x expectedKeyframesPerSecond <= 0.5`, with `expectedKeyframesPerSecond = monitors x 0.3 / (capture.intervalMs / 1000)` | `R_tokens_budget` |
| `capture.intervalMs` | when even 70 tokens break the duty rule, the smallest of 2000, 3000, 5000, 10000 that satisfies it | `R_interval_slow_inference` |
| `replay.encoder`, `replay.encoderAdapter` | the first of av1, hevc, h264 whose P95 encode time at the chosen layout, size and fps is under 50% of the frame period, on the adapter that is not driving a game (integrated first) | `R_encoder_realtime` |
| `replay.layout` | `composited` when it passes the realtime rule at native size; else `perMonitor` when that passes; else `composited` with a lower resolution | `R_layout` |
| `replay.fps` | the highest of 60, 30, 24, 15 (and the monitor refresh rate when 120 or more and the encoder has headroom under 25% of the period) that passes the realtime rule and keeps agent CPU under 1% of one core on a static screen | `R_fps_headroom` |
| `replay.resolution` | `native` when it passes; else the largest of 1440p, 1080p, 720p that passes | `R_resolution_realtime` |
| `replay.bitrateKbps` | `pixels x fps x bpp / 1000`, bpp 0.035 (av1), 0.05 (hevc), 0.08 (h264), clamped to 500..50000, rounded to 500 | `R_bitrate_quality` |
| `replay.seconds` | 60 when RAM under 8 GB, else unchanged | `R_seconds_ram` |
| `replay.enabled` | false when no adapter passes at 720p 15 fps | `R_replay_unsupported` |
| `index.dim` | 512 when RAM under 8 GB and retention over 30 days, else unchanged | `R_dim_ram` |
| `governance.pauseOnBattery` | true when the machine has a battery | `R_battery` |

Reason `Args` order (Core `Recommender` sends, UI `R_*` strings consume):

| Code | Args |
|---|---|
| `R_device_fastest` | device, image ms |
| `R_tokens_budget` | tokens, image ms, keyframes per second, duty % |
| `R_interval_slow_inference` | interval ms, image ms at 70 tokens, keyframes per second, duty % |
| `R_encoder_realtime` | codec, adapter, P95 ms, frame period ms |
| `R_layout` | layout, resolution |
| `R_resolution_realtime` | resolution, width, height, P95 ms |
| `R_fps_headroom` | fps, P95 ms, frame period ms, CPU % |
| `R_bitrate_quality` | kbps, width, height, fps, bits per pixel |
| `R_seconds_ram`, `R_dim_ram` | RAM MB (`R_dim_ram` adds retention days) |
| `R_replay_unsupported` | resolution, fps tried |
| `R_battery` | none |

The UI shows each recommendation with current value, proposed value and the localised reason (args filled in), plus the raw numbers (tables of encoder and inference results) in an expandable section. First run offers the benchmark after consent; the user can skip it.
