# BetterRecall: design

Written 2026-11-10. Design only. No application code exists yet.

This file records the design the user set on 2026-11-10. `docs\v1-design.md` is the build spec that implements it and holds every setting key and protocol. It replaces the stack summary in `docs\research.md` section 0 where the two differ. Notes marked "Review" are checks against the research that the design must answer before build. Claims marked "unverified" have not been tested on the target machine.

## Contents

1. Goal
2. Hardware allocation
3. Ingest and in-memory replay buffer
4. Local AI and semantic indexing
5. Local encryption
6. Privacy and content filtering
7. What persists and what does not
8. Review notes against the research
9. Changes from research.md

## 1. Goal

A privacy-first, hardware-accelerated local alternative to Microsoft Recall with two parts:

- An "Instant Replay" buffer: the last 60 to 120 s of screen and desktop audio, held only in RAM, exported to MP4 on a hotkey.
- An on-device semantic search engine over keyframes, audio transcripts and window metadata, stored encrypted. No OCR (decided 2026-11-10).

## 2. Hardware allocation

Target machine: Intel Core Ultra 7 265K with discrete GPUs RX 6600 and RTX 3050.

| Component | Role | Reason |
|---|---|---|
| 265K iGPU | Quick Sync AV1 hardware encoder (`av1_qsv`) | Native AV1 encode, keeps the dGPUs free |
| 265K NPU (Intel AI Boost) | OpenVINO inference of EmbeddingGemma 2 | Keeps CPU clocks and GPU rendering untouched |
| System RAM | 60 to 120 s circular replay buffer | No continuous SSD writes |
| RX 6600, RTX 3050 | Primary display, games, CUDA | Not used by the recorder |

## 3. Ingest and in-memory replay buffer

- Screen capture with DXGI Desktop Duplication. Desktop audio with WASAPI loopback, synchronised to video by QPC timestamps.
- Both streams write into a circular ring buffer in RAM that holds the last 60 to 120 s.
- Video is encoded on the iGPU with Quick Sync AV1 only. Real-time AV1 cuts the raw stream by more than 99%, so a 2-minute buffer fits under 100 MB of RAM.
- GOP length is fixed at 1 s (one keyframe per second, no open GOP). The replay hotkey writes an MP4 cut at a keyframe boundary without re-encoding.

## 4. Local AI and semantic indexing

- Keyframes are sampled every 1 to 3 s, or on scene change, and embedded with EmbeddingGemma 2 compiled for the 265K NPU through OpenVINO.
- Vectors are stored at the full 768 dimensions by default. Setting `index.dim` lets the user pick 768, 512 or 256 (Matryoshka truncation, then L2 renormalisation). 128-d is not offered (review note 1).
- Each row also stores a binary copy for the first-stage scan (96, 64 or 32 B), then the top candidates are reranked with the float vector.
- Each row records `model_id`, `revision` and `dim`. A query vector is truncated to each row group's `dim` before comparison, so rows of different sizes stay searchable together.
- Lowering `index.dim` truncates stored vectors in place, no re-embed needed, and frees space after `VACUUM`. Raising it applies to new rows only: old keyframe pixels are gone from the RAM buffer, so old rows cannot be re-embedded unless persistent keyframes are on. The settings page says this before the change.

| `index.dim` | Float per row | Binary per row | 1M rows, float + binary | 5M rows, binary scan |
|---|---|---|---|---|
| 768 (default) | 3,072 B | 96 B | 3.2 GB | 480 MB |
| 512 | 2,048 B | 64 B | 2.1 GB | 320 MB |
| 256 | 1,024 B | 32 B | 1.1 GB | 160 MB |
- No OCR. A keyframe is found by its EmbeddingGemma 2 image vector. Window titles, app names and audio transcripts are kept as text, so keyword search still covers them. Exact strings that appear only inside a frame are found only as well as the image vector captures them.

## 5. Local encryption

- Database: vectors, transcripts, window titles and timestamps live in a SQLCipher-format database with AES-256 page encryption.
- Blobs: exported clips and keyframes are encrypted with AES-256-GCM before they reach disk.
- Keys: a random 256-bit master key encrypts the database and blobs. A non-exportable RSA key in the TPM (Microsoft Platform Crypto Provider) wraps it, and Windows Hello gates each unwrap, with a session timeout. Fallbacks, in order: a passphrase-derived key with Argon2id, then Windows DPAPI (CurrentUser) when there is no TPM or Hello. The UI shows which mode is active (review note 9).

## 6. Privacy and content filtering

- Private tab detection: UI Automation inspects Chromium toolbars (Chrome, Brave, Edge) for the Incognito, InPrivate or Private badge. On a hit, the recorder masks that window's rectangle or pauses capture.
- NSFW gate: keyframes pass through a small INT8 vision classifier before EmbeddingGemma 2. Explicit frames are dropped and never indexed.
- RAM-only ephemerality: nothing in the replay buffer reaches disk unless the user exports it. Unexported data is overwritten as the buffer rolls forward.
- Panic flush: a global hotkey clears the RAM queues, zeroes the buffers and runs a truncating WAL checkpoint on the database so no remnants stay in the write-ahead log.

## 7. What persists and what does not

| Data | Location | Lifetime |
|---|---|---|
| Encoded video and audio | RAM ring buffer | 60 to 120 s, then overwritten |
| Exported replay clip | Disk, AES-256-GCM | Until the user deletes it |
| Keyframe images (one per 2 s, after de-duplication) | Encrypted, on disk | Retention setting (30 days). On by default, `keyframes.enabled` turns it off |
| Preview thumbnails | Agent RAM cache only | Made on demand from the keyframe when the UI shows a result, never written to disk |
| Vectors, transcripts, titles, process names, timestamps | Encrypted SQLite | Retention setting |

Video is RAM-only. Keyframe images persist by default, so an old search hit still shows an image. With `keyframes.enabled` off, a hit older than the replay buffer returns time, app and title only.

## 8. Review notes against the research

Each note names the design point, what the research found, and what to do.

1. MRL 128 dimensions. Applied 2026-11-10: the design now stores 768-d by default with 512 and 256 as user options, and drops 128. The EmbeddingGemma 2 model card says 128-d degrades multimodal retrieval. No published 1M-row brute-force latency exists for 768-d (research section 4.1), so measure it. If the 768-d scan is too slow at 5M rows, 256-d scans in about a third of the time, or a USearch sidecar index can be added.
2. EmbeddingGemma 2 on the NPU. Not proven on this NPU generation. On Meteor Lake (same NPU 3720 as Arrow Lake) the vision tower failed to compile, and on Wildcat Lake one image took 1.3 s at 140 tokens and 0.65 s at 70 tokens. Keyframes every 1 to 3 s would keep the NPU near full duty. Sample on scene change with de-duplication (research section 3.1), use 70 or 140 image tokens, and keep SigLIP 2 base on the NPU as the fallback. Test first (research section 8, test 1).
3. "Bypassing the CPU and GPU entirely." Preprocessing (resize, normalise) and tokenisation still run on the CPU, and the vision tower's output feeds the text transformer. Expect a small CPU cost, not zero.
4. Cross-adapter copy. The monitors are driven by the RX 6600 or RTX 3050, so Desktop Duplication returns frames in dGPU memory. The AV1 encoder is on the iGPU. Every frame must cross PCIe: about 15 MB per 1440p frame and 33 MB per 4K frame in BGRA. At 30 fps that is 0.4 to 1 GB/s per monitor. Downscale or convert to NV12 on the dGPU first (a quarter to half the bytes), cap the frame rate (for example 15 fps), and measure. This is the largest unmeasured cost in the design. The iGPU must stay enabled in firmware with no monitor attached (unverified that QSV runs headless on this board).
5. AV1 encode on the 265K. Arrow Lake desktop uses the Xe-LPG media engine, which lists AV1 encode. Unverified on this machine. Check with `ffmpeg -hide_banner -h encoder=av1_qsv` and a 120 s test encode.
6. "Under 100 MB for 2 minutes." Holds at about 6 Mbps or less (6 Mbps x 120 s = 90 MB). One 1440p screen at 15 to 30 fps fits. Several monitors or 4K need a per-monitor bitrate cap or a shorter buffer. Add the audio buffer (Opus at 128 kbps is about 2 MB per 2 minutes).
7. 1-second GOPs. Cutting at a keyframe without re-encoding works, but the cut can start up to 1 s before the requested time. Write fragmented MP4 so the export is a byte copy plus a header.
8. SQLCipher. The free SQLCipher build for .NET was removed in SQLitePCLRaw 3.0 and official SQLCipher binaries are commercial. SQLite3MultipleCiphers (`SQLite3MC.PCLRaw.bundle` 2.4.0) has a SQLCipher-compatible cipher scheme (AES-256-CBC with HMAC-SHA512). Select that scheme to keep the SQLCipher file format for free. sqlite-vec under it is still unverified.
9. DPAPI and the TPM. Applied 2026-11-10: the design now wraps the key with the TPM behind Windows Hello. DPAPI is not TPM-bound. It derives from the user's logon credentials, and any process running as the same user can unseal. Use RSA on the Platform Crypto Provider (ECDSA has a known bug there). Windows Hello has no unwrap API, so either gate the TPM unwrap with `UserConsentVerifier` or derive the key from a `KeyCredentialManager` signature over a fixed challenge (research section 4.3). Same-user malware can still ask for an unwrap while the session is unlocked. `KeyCredentialManager` in an unpackaged build is unverified.
10. Private tab detection through UI Automation. This is the research's option (c) and is unverified. It is per browser and per browser version, and badge text is localised. Pair it with window-title matching ("InPrivate", "Incognito", "Private Browsing") and a "never capture this browser" switch. Masking a rectangle is only safe if the mask is applied before encoding. If the badge check fails mid-frame, drop the frame.
11. RAM-only claim. RAM can still reach disk four ways: the pagefile, the hibernation file, crash dumps and the GPU driver's own paging. Allocate the ring buffer with native memory (not the .NET managed heap, which copies on compaction), lock it with `VirtualLock` after raising the working-set minimum with `SetProcessWorkingSetSizeEx`, exclude the process from Windows Error Reporting dumps (`WerAddExcludedApplication`), and say in the docs that hibernation writes RAM to `hiberfil.sys` (BitLocker covers that file). Encoder surfaces in iGPU memory cannot be locked from user mode.
12. Panic flush. `PRAGMA wal_checkpoint(TRUNCATE)` empties the WAL, but freed pages in the main file keep old ciphertext unless `PRAGMA secure_delete=ON` is set. Since the database is encrypted, leftovers are ciphertext. Define exactly what panic flush deletes: RAM only, or RAM plus the last N minutes of index rows.
13. NSFW classifier. Pick a model with a permissive licence and run it INT8 QDQ on CPU or iGPU, not on the NPU, to leave NPU time for EmbeddingGemma 2. Research found INT8 weight-only compression breaks EmbeddingGemma 2 vision on the NPU. That finding does not apply to a separate classifier with proper QDQ calibration, but check its accuracy after quantisation.
14. Window titles in the index. Titles often contain private data (email subjects, document names). They are covered by database encryption, but they bypass the NSFW gate. Run the deny lists and the text filter on titles too.

## 9. Changes from research.md

| Topic | research.md | This design |
|---|---|---|
| Capture model | Snapshot every 5 to 10 s, optional thumbnails | Continuous AV1 video in a RAM ring buffer. Keyframe every 2 s for indexing, kept encrypted by default. Preview thumbnails made on demand |
| Video encode | None | Quick Sync AV1 on the iGPU, 1 s GOP |
| Runtime | ONNX Runtime with plugin EPs, OpenVINO native as fallback | OpenVINO on the NPU for EmbeddingGemma 2 (the research's fallback becomes the main path on this machine). ONNX Runtime kept for non-Intel NPUs and GPUs |
| Device order | Measured, iGPU may win | NPU fixed for embeddings, iGPU reserved for encode |
| Vector size | 768-d | 768-d default, user option 512 or 256 via Matryoshka, binary prefilter plus float rerank (note 1) |
| Database cipher | SQLite3MultipleCiphers default (ChaCha20-Poly1305) | SQLCipher format (see note 8) |
| Key sealing | TPM key behind Windows Hello, DPAPI fallback | Same, plus Argon2id passphrase as the first fallback (note 9) |
| Private windows | Per-browser exclusion, best-effort titles | UI Automation badge detection with masking |
| Content filter | Regex filter on OCR text | INT8 NSFW image classifier before indexing. No OCR at all |
| Panic control | Pause hotkey, purge via UI | Panic-flush hotkey |

Unchanged from research.md: Desktop Duplication, WASAPI loopback, SQLite with FTS5 and sqlite-vec, the three-process split, WinUI 3, the optional IPC and MCP design in section 7.
