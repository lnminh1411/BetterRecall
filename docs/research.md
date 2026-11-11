# BetterRecall: technical research

Written 2026-11-10. Research only. No application code exists yet.

Method: four parallel web-research passes over primary sources (vendor docs, model cards, release pages, issue trackers), then one merge pass. Versions and dates are stated where the source gave them. A claim marked "unverified" has no primary source or was not tested on the target machine (Core Ultra 7 265K, Arrow Lake, Intel AI Boost NPU, Windows 11). Benchmarks from community model cards and GitHub issues are secondary evidence and say so.

## Contents

0. Summary of the recommended stack
1. Embedding model
2. Inference runtime
3. Capture
4. Storage
5. Process architecture
6. Prior art
7. Optional IPC and MCP
8. Open risks and decisions

## 0. Summary of the recommended stack

The user set a newer design on 2026-11-10 (RAM replay buffer, Quick Sync AV1, OpenVINO on the NPU, SQLCipher format, NSFW gate). `docs\design.md` holds it and wins where it differs from this table. Its section 8 checks the design against these findings.

| Layer | Choice | Fallback |
|---|---|---|
| Visual + text embedding | EmbeddingGemma 2 (`google/embeddinggemma-2`), text and vision towers only, 768-d, 140 image tokens, static-shape graphs | SigLIP 2 base (Apache 2.0), also the A/B baseline |
| Audio | Silero VAD, then local Whisper large-v3-turbo int8, then text embedding. No direct audio embedding in v1 | PhoWhisper-large (Vietnamese), sherpa-onnx Zipformer on CPU |
| Text beside vectors | UI Automation text first, `Windows.Media.Ocr` when text is thin, stored in SQLite FTS5 and fused with vectors by reciprocal rank fusion | RapidOCR PP-OCRv5 |
| Inference runtime | ONNX Runtime with bundled plugin execution providers (OpenVINO on Intel, QNN on Snapdragon, DirectML for any GPU), loaded through the self-contained Windows ML package or plain `Microsoft.ML.OnnxRuntime` | OpenVINO native 2026.4.1 through its C API, Intel only |
| Device order | Measure at first run, cache the winner. Default order NPU, GPU, CPU. On Arrow Lake the iGPU may win | User override in settings |
| Screen capture | DXGI Desktop Duplication, one duplication per output | Windows.Graphics.Capture |
| Audio capture | WASAPI process loopback that excludes the app's own process tree, optional microphone, both off by default | None |
| Guardrails | Foreground-event gate plus shot-time gate: session lock, display affinity, process deny list, "exclude this browser" switch, pause hotkey | OCR-text sensitive filter as a last layer only |
| Store | SQLite (WAL) + FTS5 + sqlite-vec 0.1.9, brute-force two-stage search (binary scan, int8 or float rerank), month partitions | USearch sidecar index at 5M vectors |
| Encryption | SQLite3MultipleCiphers whole-database encryption. Random 256-bit data key wrapped by a TPM key, released behind Windows Hello | DPAPI CurrentUser when no TPM or Hello |
| Processes | Three per-user processes: headless Native AOT `agent`, on-demand `worker` for inference, on-demand WinUI 3 `ui`. No Windows service | None |
| UI | WinUI 3 on Windows App SDK 2.x, .NET 10 LTS | None |
| Packaging | MSIX, or external-location package identity, for the startup task and capture capabilities | Unpackaged developer mode with Desktop Duplication only |
| IPC | Named pipe `\\.\pipe\BetterRecall`, `ipc.enabled` default false, line-delimited JSON, current-user-only DACL | None |
| MCP | Separate stdio process that is a client of the same pipe, `mcp.enabled` default false | None |

Things in the brief that the research changed:

- LiteRT and LiteRT-LM cannot be the runtime. LiteRT's Intel NPU support covers Lunar Lake and Panther Lake only, and it has no C# binding.
- "NPU first" is a default, not a rule. Secondary benchmarks show the Arrow Lake iGPU about 3x faster than its NPU on vision models, and nobody has shown the EmbeddingGemma 2 vision tower compiling on this NPU generation.
- LanceDB and DuckDB do not fit. LanceDB has no official .NET SDK and no local encryption. DuckDB's HNSW persistence is still experimental.
- SQLCipher is no longer free in the .NET bundle. SQLite3MultipleCiphers replaces it.
- A Windows service cannot capture the screen (session 0). The daemon must be a per-user process.
- No confirmed way exists to detect a private browser window from another process.
- Direct audio embedding does not run on this NPU class. Transcribe first.

## 1. Embedding model

### 1.1 The "Embedding Gemma 2" claim

The claim is correct. EmbeddingGemma 2 exists and is multimodal.

| Fact | Value | Source |
|---|---|---|
| Model id | `google/embeddinggemma-2` | [1] |
| Released | 2026-10-06, Google DeepMind | [2] [3] |
| Base | Gemma 4 | [1] |
| Parameters | 740M total: text 270M, vision 170M, audio 300M. One outlet says 744M | [1] [3] |
| Selective load | Text + vision is 440M | [1] |
| Output | One shared space, 768-d, Matryoshka truncation to 512, 256, 128. The card says 128-d degrades multimodal retrieval | [1] |
| Context | 8,192 tokens | [1] |
| Image cost | 280 tokens by default, adjustable 70 to 1120. Video is 140 tokens per frame | [1] [4] |
| Audio cost | 25 tokens per second, 16 kHz mono, about 327 s maximum | [1] |
| Languages | 100+. The card does not name Vietnamese | [1] |
| Licence | Apache 2.0 | [1] |
| Benchmarks | MTEB multilingual v2 61.36, MTEB code 78.68, MMEB v2 image 57.28, MSEB 69.54 | [1] |
| Known trap | The card warns that float16 inference produces NaN | [1] |

Do not confuse it with Gemini Embedding 2 (2026-03-10, 3072-d), which is a cloud-only API model [5] [6].

Available formats:

| Format | Repo | Status |
|---|---|---|
| safetensors | `google/embeddinggemma-2` | Official [1] |
| LiteRT-LM, 485 MB | `litert-community/embeddinggemma-2-740m-litert-lm` | Official community org [7] |
| ONNX, 6.17 GB | `onnx-community/embeddinggemma-2-ONNX` | Community [8] |
| OpenVINO IR int8 | `wesjos/embeddinggemma-2-openvino-int8` | Community [9] |
| Static vision ONNX for NPU | `arankays/embeddinggemma-2-vision-static-onnx` | Community [10] |
| DirectML ONNX | `bep256/embeddinggemma-2-ONNX` | Community [11] |
| AMD NPU | `brishen/iron-embeddinggemma2-npu2` | Community [12] |
| QNN | `znasif/embeddinggemma-2-qnn-v69` | Community [13] |

Intel NPU evidence, all secondary:

- Google's published NPU latency of 108.6 ms is from a Panther Lake Dell XPS 16 through LiteRT [4]. LiteRT's Intel NPU path supports Lunar Lake and Panther Lake only [14].
- Arrow Lake uses the NPU 3720 generation, the same as Meteor Lake [15].
- Wildcat Lake NPU, static vision graph: 2767, 1311, 653 ms per image at 280, 140, 70 tokens. First compile 95, 28, 12 s. Cosine against the reference at least 0.9994. Flickr30k R@1 86.3, 84.5, 81.1. INT8 weight compression broke the vision tower on NPU (cosine 0.67) [10].
- Meteor Lake 125H: text tower on NPU 37.1 ms at 1x128 tokens against 65.1 ms on CPU. The vision tower failed to compile (`ConvertReduceToPooling Pass failed`). Audio was unusable. One image on CPU took 2.7 s [9].
- The vision tower's output feeds the text transformer, so each image costs two passes [1].

Unverified: the EmbeddingGemma 2 vision tower on Arrow Lake NPU 3720, and its Vietnamese retrieval quality.

### 1.2 Candidates

| Model | Params | Dim | Matryoshka | Licence | Multilingual text | NPU-ready exports | Verdict |
|---|---|---|---|---|---|---|---|
| EmbeddingGemma 2 | 440M (text + vision) | 768 | Yes | Apache 2.0 | Yes, 8,192 tokens | Community static ONNX and OpenVINO IR | Primary |
| SigLIP 2 base patch16 | about 375M | 768 | No | Apache 2.0 | Yes, 64-token text | ONNX [16], OpenVINO IR for so400m [17] | Fallback and baseline. so400m runs 428 ms per image at 512x512 on Meteor Lake NPU [17] |
| MobileCLIP2-S0 | small | 512 | No | apple-amlr (research) [18] | English | CoreML, OpenCLIP | Rejected, licence |
| Jina CLIP v2 | 865M | 1024 | Yes | CC-BY-NC [19] | Yes | ONNX | Rejected, licence |
| jina-embeddings-v4 | 3.8B | 2048 | Yes | Qwen research [20] | Yes | None for NPU | Rejected, licence and size |
| Nomic embed vision + text v1.5 | 92M + 137M | 768 | Yes (text) | Apache 2.0 [21] | English | ONNX | Rejected, language |
| TinyCLIP | 39M + 19M | 512 | No | MIT | English | ONNX | Rejected, language |
| OpenAI CLIP, EVA02-CLIP | varies | 512 to 1024 | No | MIT | English | ONNX, Qualcomm AI Hub [22] | Rejected, language |
| ImageBind | 1.2B | 1024 | No | CC-BY-NC-SA [23] | English | None | Rejected, licence |
| ColPali family, nomic-embed-multimodal | 3B to 7B | multi-vector | No | varies | Yes | None | Rejected, too heavy for a 5 to 10 s loop [24] [25] |
| CLAP (audio) | about 200M | 512 | No | MIT [26] | English captions | ONNX | Later, non-speech audio only |

Microsoft Recall names four on-device models: Screen Region Detector, OCR, Natural Language Parser, Image Encoder [27] [28]. It does not embed audio.

Intel NPU constraints that apply to every candidate [15] [29] [30]:

- Static input shapes. Export one graph per fixed resolution and token count.
- FP16 compute on the NPU. Test EmbeddingGemma 2 for the NaN the card warns about.
- First compile is slow. Cache the compiled blob. The cache breaks across driver and OpenVINO versions, so key it on both.
- INT8 needs QDQ quantization with calibration. Weight-only compression is not safe for vision towers.

### 1.3 Audio path

Recommendation: transcribe locally, then embed the text. Direct audio embedding fails on this NPU class [9] and adds 300M parameters.

| Stage | Choice | Evidence |
|---|---|---|
| Gate | Silero VAD v6, CPU, under 1 ms per 30 ms chunk | [31] [32] |
| ASR | Whisper large-v3-turbo int8 through OpenVINO GenAI `WhisperPipeline` on NPU [33] | On a 265K under Linux with OpenVINO GenAI 2026.4.1: 13.7 s clip in 1.64 s on NPU, 3.0 s on CPU, 7.8 to 10 s on iGPU. First compile about 2 min [34]. Arrow Lake-H on Windows: cold compile 238 to 279 s, 5.6 to 6.6x real time [35] |
| Vietnamese | PhoWhisper-large int8 OpenVINO [36] | Lower Vietnamese WER than Whisper in its paper. Unverified on this machine |
| CPU option | sherpa-onnx Zipformer (Vietnamese 30M) or Moonshine base | [37] [38] |

Run one NPU queue and load one model per process. Whisper and the image embedder must not compile or run at the same time.

Unverified: Whisper numbers on Windows for the 265K, and Whisper turbo Vietnamese WER.

### 1.4 OCR text beside the visual embedding

Yes, store it. Recall, screenpipe, OpenRecall and Windrecorder all keep text and search it [28] [39] [40] [41]. Exact strings (file names, error codes, names) are found by keyword search, not by a 768-d image vector.

| Text source | Cost | Notes |
|---|---|---|
| UI Automation tree | Near zero | Use first. screenpipe moved to accessibility text first for this reason [39] |
| `Windows.Media.Ocr` | CPU, tens of ms | In-box. Needs a language pack per language [42]. A Vietnamese OCR pack is unverified. Check with `Get-WindowsCapability -Online \| Where-Object Name -Like 'Language.OCR*'` |
| RapidOCR PP-OCRv5 (ONNX, `RapidOcrNet`) | CPU or GPU | Second engine [43] [44]. Reports on Vietnamese tone marks conflict |
| Windows AI `TextRecognizer` | NPU | Copilot+ PCs only [45]. Not available on the 265K |

Store text in FTS5 in the same database. At query time run FTS5 and vector search, then merge with reciprocal rank fusion.

### 1.5 Recommendation

Use EmbeddingGemma 2, text and vision towers, 768-d, 140 image tokens, exported as static-shape graphs. Reason: it is the only Apache-licensed model that puts long multilingual text and images in one space, so OCR text, transcripts and frames share one index. Keep SigLIP 2 base as the fallback and as the comparison baseline, because its NPU path is proven on Meteor Lake and EmbeddingGemma 2's vision tower is not. Store `model_id`, `revision` and `dim` on every vector so a model change triggers a re-embed instead of mixing spaces. Transcribe audio, do not embed it. Store OCR and accessibility text for hybrid search.

### Sources for section 1

1. https://huggingface.co/google/embeddinggemma-2
2. https://blog.google/innovation-and-ai/technology/developers-tools/embeddinggemma-2/
3. https://siliconangle.com/2026/10/06/google-expands-embeddinggemma-beyond-text-to-images-audio-and-video/
4. https://developers.googleblog.com/embeddinggemma-2-the-developer-guide/
5. https://blog.google/innovation-and-ai/models-and-research/gemini-models/gemini-embedding-2/
6. https://developers.googleblog.com/building-with-gemini-embedding-2/
7. https://huggingface.co/litert-community/embeddinggemma-2-740m-litert-lm
8. https://huggingface.co/onnx-community/embeddinggemma-2-ONNX
9. https://huggingface.co/wesjos/embeddinggemma-2-openvino-int8
10. https://huggingface.co/arankays/embeddinggemma-2-vision-static-onnx
11. https://huggingface.co/bep256/embeddinggemma-2-ONNX
12. https://huggingface.co/brishen/iron-embeddinggemma2-npu2
13. https://huggingface.co/znasif/embeddinggemma-2-qnn-v69
14. https://developers.google.com/edge/litert/next/intel
15. https://github.com/openvinotoolkit/openvino/blob/master/src/plugins/intel_npu/README.md
16. https://huggingface.co/onnx-community/siglip2-base-patch16-naflex-ONNX
17. https://huggingface.co/wesjos/siglip2-so400m-patch16-512-openvino
18. https://huggingface.co/apple/MobileCLIP2-S0/blame/main/LICENSE
19. https://huggingface.co/jinaai/jina-clip-v2
20. https://jina.ai/models/jina-embeddings-v4/
21. https://huggingface.co/nomic-ai/nomic-embed-vision-v1.5
22. https://huggingface.co/qualcomm/OpenAI-Clip
23. https://github.com/facebookresearch/ImageBind/blob/main/model_card.md
24. https://huggingface.co/vidore/colqwen2-v0.1
25. https://huggingface.co/nomic-ai/nomic-embed-multimodal-7b
26. https://github.com/microsoft/CLAP
27. https://blogs.windows.com/windowsdeveloper/2024/05/21/unlock-a-new-era-of-innovation-with-windows-copilot-runtime-and-copilot-pcs/
28. https://blogs.windows.com/windowsexperience/2024/09/27/update-on-recall-security-and-privacy-architecture/
29. https://docs.openvino.ai/2026/openvino-workflow/running-inference/inference-devices-and-modes/npu-device.html
30. https://onnxruntime.ai/docs/execution-providers/OpenVINO-ExecutionProvider.html
31. https://github.com/snakers4/silero-vad/wiki/Quality-Metrics
32. https://huggingface.co/bitsydarel/silero-vad-onnx
33. https://huggingface.co/bweng/whisper-large-v3-turbo-int8-ov
34. https://github.com/cjpais/Handy/pull/2361
35. https://github.com/hlevring/ov-genai-speed-test/blob/main/results/npu/npu-benchmark-summary%20-%20Arrow%20Lake.md
36. https://huggingface.co/jermalk/PhoWhisper-large-int8-ov
37. https://k2-fsa.github.io/sherpa/onnx/index.html
38. https://arxiv.org/pdf/2509.02523
39. https://docs.screenpipe.com/architecture
40. https://github.com/openrecall/openrecall
41. https://github.com/yuka-friends/Windrecorder
42. https://learn.microsoft.com/en-us/uwp/api/windows.media.ocr.ocrengine.availablerecognizerlanguages?view=winrt-26100
43. https://github.com/BobLd/RapidOcrNet
44. https://github.com/PaddlePaddle/PaddleOCR/blob/main/docs/version3.x/algorithm/PP-OCRv5/PP-OCRv5_multi_languages.en.md
45. https://learn.microsoft.com/en-us/windows/ai/apis/text-recognition

## 2. Inference runtime

### 2.1 Options

| Option | Version, date | NPU vendors | GPU coverage | .NET binding | Packaging | Model format | Device selection and fallback | Arrow Lake NPU |
|---|---|---|---|---|---|---|---|---|
| Windows ML with catalog EPs | `Microsoft.Windows.AI.MachineLearning` 2.5.96, 2026-10-26, ships ORT 1.28.3 [1] [2] | Intel, Qualcomm, AMD through downloaded EPs [3] | DirectML in box, vendor GPU EPs downloaded | Official C#, C++ | About 41 MB self-contained. EPs download from the Microsoft Store on first use and need Windows 11 24H2 [4] [5] | ONNX | `EnsureAndRegisterCertifiedAsync`, then selection policy | Works, but the EP download breaks the zero-outbound rule |
| ONNX Runtime with bundled plugin EPs | ORT 1.31.1, 2026-10-26 [6]. `Intel.ML.OnnxRuntime.EP.OpenVINO` 1.8.1, 2026-10-26, 121 MB, x64, needs ORT 1.23.0+ [7] | Intel (OpenVINO), Qualcomm (QNN) [8]. AMD VitisAI is not bring-your-own and needs INT8 or BF16 plus Ryzen AI Software [9] [10] | Intel GPU through OpenVINO, any DX12 GPU through DirectML [11] | Official C# | Ship the EP DLLs in the app. No network | ONNX | `RegisterExecutionProviderLibrary`, `GetEpDevices`, `AppendExecutionProvider_V2` or `SetEpSelectionPolicy(PREFER_NPU)` [12] [13] | Supported through the OpenVINO EP, static shapes |
| OpenVINO native + GenAI | 2026.4.1, 2026-10-01 [14] | Intel only | Intel only | No official binding. Community `JYPPX.OpenVINO.CSharp.API` 3.3.0 [15], or P/Invoke on the C API | Ship the runtime, about 100+ MB | OpenVINO IR, ONNX | `AUTO:NPU,GPU,CPU` device string, performance hints [16] [17] | Supported, NPU 3720 [18] |
| LiteRT / LiteRT-LM | LiteRT 2.3.2 [19] | Intel Lunar Lake and Panther Lake only, Qualcomm, MediaTek [20] | WebGPU, OpenCL [21] | None for C# | C++ library | `.tflite`, `.litertlm` | Per-accelerator compiled model | Not supported |
| Foundry Local | n/a | Through its own service | Yes | REST | Separate service, model catalog download [22] | ONNX GenAI | Service decides | Rejected. It is a model server with a network catalog |

Facts that shape the choice:

- Catalog EPs in Windows ML come from the network [4]. The bring-your-own-EP path is documented and keeps everything in the installer [23] [24]. Never call `EnsureReadyAsync()` or `EnsureAndRegisterCertifiedAsync()`.
- The OpenVINO plugin EP registers as `OpenVINOExecutionProvider` and takes OpenVINO properties through `load_config` [7] [25].
- QNN uses `Microsoft.ML.OnnxRuntime.QNN`, `backend_path` set to `QnnHtp.dll`, and prefers QDQ-quantized models [8].
- DirectML is in sustained engineering: fixes only, no new features [11]. It remains the one EP that runs on every vendor's GPU.
- Intel NPU driver 32.0.100.5540 (2026-08-28) is current [26] [27].
- Secondary benchmarks put the iGPU ahead of the NPU on this chip family: D-FINE 43.8 ms on NPU against 15.1 ms on GPU (225H) [28], Frigate detection 12 to 20 ms against about 4 ms (245K) [29], MiniLM on Arrow Lake NPU about equal to CPU [30].

### 2.2 Model preparation rules

- One static-shape graph per model, per resolution and token count.
- Ship an FP32 master plus FP16 and INT8 QDQ variants. Choose per device at first run.
- Cache compiled blobs or EPContext models under a key of EP version, driver version and model hash. Recompile when any part changes.
- Verify each device's output against the CPU reference by cosine similarity (0.999 or better) before accepting it. This catches the float16 NaN and the INT8 vision failure from section 1.

### 2.3 Background-inference throttling

- Run the worker under EcoQoS (`SetProcessInformation` with `ProcessPowerThrottling`) [31] [32].
- OpenVINO: `NPU_TURBO` off, one inference request, `GPU_QUEUE_THROTTLE` LOW on the iGPU [18] [33]. Use OpenVINO 2026.2.1 or later.
- Windows ML efficiency guidance: batch work, release sessions when idle [34].

### 2.4 Recommendation

Primary: ONNX Runtime with plugin execution providers bundled in the installer. Load it through the self-contained Windows ML package if a packet capture shows no network calls on the bring-your-own path, otherwise through plain `Microsoft.ML.OnnxRuntime`. Reason: one model format, official C# API, Intel and Qualcomm NPUs plus every GPU, and nothing downloaded at run time.

Fallback: OpenVINO native 2026.4.1 through its C API, Intel only. Reason: it is the layer under the OpenVINO EP, it exposes the NPU properties directly, and Whisper on NPU already needs OpenVINO GenAI.

Device order: NPU (OpenVINO EP on Intel, QNN on Snapdragon), then GPU (OpenVINO on Intel GPU, DirectML elsewhere, which covers AMD), then CPU. Treat that order as the starting guess. At first run, time each device on the real model, keep the fastest that passes the cosine check, cache the result, and let the user override it. On the 265K the iGPU will probably win for vision.

Unverified: whether Windows ML makes network calls when no catalog API is called, whether plugin EP 1.8.1 and DirectML coexist with ORT 1.28.3 in one process, and SigLIP or CLIP latency and compile time on NPU 3720.

### Sources for section 2

1. https://learn.microsoft.com/en-us/windows/ai/new-windows-ml/overview
2. https://learn.microsoft.com/en-us/windows/ai/new-windows-ml/onnx-versions
3. https://learn.microsoft.com/en-us/windows/ai/new-windows-ml/supported-execution-providers
4. https://learn.microsoft.com/en-us/windows/ai/new-windows-ml/initialize-execution-providers
5. https://learn.microsoft.com/en-us/windows/ai/new-windows-ml/distributing-your-app
6. https://github.com/microsoft/onnxruntime/releases
7. https://www.nuget.org/packages/Intel.ML.OnnxRuntime.EP.OpenVINO
8. https://onnxruntime.ai/docs/execution-providers/QNN-ExecutionProvider.html
9. https://onnxruntime.ai/docs/execution-providers/Vitis-AI-ExecutionProvider.html
10. https://ryzenai.docs.amd.com/en/latest/inst.html
11. https://onnxruntime.ai/docs/execution-providers/DirectML-ExecutionProvider.html
12. https://onnxruntime.ai/docs/execution-providers/plugin-ep-libraries/usage.html
13. https://learn.microsoft.com/en-us/windows/ai/new-windows-ml/select-execution-providers
14. https://github.com/openvinotoolkit/openvino/releases
15. https://www.nuget.org/packages/JYPPX.OpenVINO.CSharp.API/3.3.0
16. https://docs.openvino.ai/2026/openvino-workflow/running-inference/inference-devices-and-modes/auto-device-selection.html
17. https://docs.openvino.ai/2026/openvino-workflow/running-inference/optimize-inference/high-level-performance-hints.html
18. https://docs.openvino.ai/2026/openvino-workflow/running-inference/inference-devices-and-modes/npu-device.html
19. https://github.com/google-ai-edge/LiteRT/releases
20. https://ai.google.dev/edge/litert/next/intel
21. https://developers.google.com/edge/litert/next/gpu
22. https://learn.microsoft.com/en-us/azure/foundry-local/concepts/foundry-local-architecture
23. https://learn.microsoft.com/en-us/windows/ai/new-windows-ml/windows-ml-eps-vs-bring-your-own
24. https://learn.microsoft.com/en-us/windows/ai/new-windows-ml/bring-your-own-eps
25. https://github.com/onnxruntime/onnxruntime-ep-openvino/blob/main/docs/QuickStart.md
26. https://www.intel.com/content/www/us/en/download/794734/intel-npu-driver-windows.html
27. https://downloadmirror.intel.com/927127/NPU_Win_Release_Notes_v32.0.100.5540.pdf
28. https://github.com/Nitidio/nitid/pull/51
29. https://github.com/blakeblackshear/frigate/discussions/13248
30. https://github.com/elizaOS/eliza/issues/7633
31. https://learn.microsoft.com/en-us/windows/win32/api/processthreadsapi/nf-processthreadsapi-setprocessinformation
32. https://github.com/MicrosoftDocs/win32/blob/docs/desktop-src/ProcThread/quality-of-service.md
33. https://docs.openvino.ai/2025/openvino-workflow/running-inference/inference-devices-and-modes/gpu-device.html
34. https://learn.microsoft.com/en-us/windows/ai/new-windows-ml/efficiency-tips

## 3. Capture

Source tags in sections 3 and 5 (W1, D2, and so on) point to the list at the end of section 5.

### 3.1 Screen capture

| Point | DXGI Desktop Duplication | Windows.Graphics.Capture (WGC) |
|---|---|---|
| Border and consent | No border, no picker, no consent prompt [D1] | Yellow border by default. Removable on build 20348+ after `GraphicsCaptureAccess.RequestAccessAsync(Borderless)`, and a packaged app declares `graphicsCaptureWithoutBorder` [S2] [S6] [C1]. Programmatic monitor capture without the picker uses `IGraphicsCaptureItemInterop::CreateForMonitor` [W2] |
| Multi-monitor | One `IDXGIOutputDuplication` per output [D3] | One capture item per monitor |
| HDR | `DuplicateOutput1` returns the native format, the app tone-maps [D4] | Frame pool in `R16G16B16A16Float` [W1] |
| DRM and protected windows | Protected content arrives black. `WDA_EXCLUDEFROMCAPTURE` windows are removed from the image [A1] | Same [A1] [A3] |
| Cost when nothing changed | `AcquireNextFrame(0)` returns `DXGI_ERROR_WAIT_TIMEOUT`. No copy, no GPU work [D2] | `MinUpdateInterval` and `DirtyRegionMode` exist from 24H2, with thin documentation [S3] [S4] |
| Change data | Dirty rectangles and move rectangles per frame [D1] | Dirty region from 24H2 only |
| Failure mode | `DXGI_ERROR_ACCESS_LOST` on desktop switch, mode change, fullscreen transition. Recreate the duplication [D2] | Session closes, recreate |
| Per-window capture | No | Yes, plus `IncludeSecondaryWindows` on 24H2 [S5] |
| Runs unpackaged | Yes | Yes, but borderless without package identity is unverified |

`WDA_EXCLUDEFROMCAPTURE` is not DRM. Any process can clear the flag on its own windows, and it only hides a window from capture APIs [A2].

Cost model at one capture every 5 s: the poll is a single call that times out when the screen is static. When a frame arrives, copy the texture once, downscale on the GPU, read back one small image for hashing and one for the encoder.

De-duplication, cheapest test first:

1. Desktop Duplication timeout: nothing changed, skip.
2. Dirty-rectangle area below a threshold (for example 1% of the screen): skip.
3. 64-bit dHash of a 9x8 grayscale thumbnail, Hamming distance below a threshold against the last kept frame: skip.
4. Near the threshold only, SSIM on a small grayscale copy. OpenRecall keeps a frame when SSIM drops below 0.9 [P5].

Add event triggers the way screenpipe does (foreground change, typing pause, scroll stop) so a capture lands when content settles instead of on a blind timer [P3] [P4]. Windrecorder also skips unchanged screens and indexes when idle [P6].

### 3.2 Audio capture

| Path | API | Notes |
|---|---|---|
| System loopback | `IAudioClient` with `AUDCLNT_STREAMFLAGS_LOOPBACK` on the render endpoint [U1] | Captures everything, including the app's own sounds |
| Process loopback | `ActivateAudioInterfaceAsync` with `AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS`, mode `PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE` [U2] [U3] [U4] [U5] | Recommended. Excludes BetterRecall's own tree. Can also include or exclude one app. Docs give build 20348 as the minimum, one page says 20438. Unverified |
| Microphone | WASAPI capture endpoint | Windows shows the microphone indicator in the tray. It cannot be hidden [U6] |
| .NET wrapper | NAudio `WasapiRecorder` [L1] [L2], or CsWin32 direct | CSCore is the older option [L3] |
| VAD | Silero VAD v6 ONNX on CPU [L4] [L5] | Gate before ASR. Drop non-speech buffers from RAM |

Protected audio streams are not delivered to loopback. Audio capture is off by default, with separate switches for system audio and microphone.

### 3.3 Privacy guardrails

Run the gate twice: on every `EVENT_SYSTEM_FOREGROUND` event from `SetWinEventHook` [G1], and again at the moment of the shot. Any hit skips the frame and drops the audio buffer.

| Layer | Check | Source |
|---|---|---|
| Session state | `WTSRegisterSessionNotification`: pause on lock, logoff, remote connect, and on `DXGI_ERROR_ACCESS_LOST` (secure desktop, UAC) | [G4] [G5] |
| Window opted out | `GetWindowDisplayAffinity(hwnd) != WDA_NONE`: skip the whole frame while such a window is visible, since the user's intent is clear. The API works from any process | [G3] [A1] |
| Process deny list | `QueryFullProcessImageNameW` on the foreground window's process. Seed with Recall's own exclusions (`mstsc.exe`, `VMConnect.exe`, Azure Virtual Desktop clients) plus password managers (1Password, Bitwarden, KeePass, KeePassXC, Dashlane, LastPass, Proton Pass). User editable | [G2] [R1] |
| Sensitive directories | Deny list on the foreground process image path prefix, and on document path when UI Automation exposes it | Design |
| Private browser windows | No confirmed cross-process detection. Recall depends on the browser calling `SetInputScope(hwnd, IS_PRIVATE)` [R2] [B1] [B2]. Brave 1.81+ sets it on every window [B3] [B4] [B5]. Signal uses content protection [B6]. Reading that flag from another process is unverified | See below |
| Manual | Pause hotkey, tray pause for 15 min, 1 h, until resume | Design |
| Text filter | Regex and checksum patterns (card numbers, keys) on OCR text. Drop the row on a hit. Recall's filter misses secrets in public tests [R3], so treat this as the last layer | [R3] |

Private-browsing plan until a test settles it: (a) a per-browser "never capture this browser" switch, (b) window-title heuristics ("InPrivate", "Incognito", "Private Browsing") marked as best effort in the UI, (c) UI Automation lookup of the browser's private-mode indicator as an experiment.

How Recall filters: app and website exclusion lists, private browsing in supported browsers, DRM content, remote desktop clients, sensitive-information filter on by default, and content labels from apps [R1] [R3] [R5].

### 3.4 Recommendation

Use Desktop Duplication as the primary capture path and WGC as the fallback. Reason: Desktop Duplication has no border and no consent prompt, costs nothing when the screen is static, and gives dirty rectangles for free. WGC covers the cases where duplication fails and per-window capture. For audio use process loopback in exclude-own-tree mode with Silero VAD, off by default. Build guardrails as layers and tell the user plainly that private-window detection is best effort.

## 4. Storage

### 4.1 Engines for 1 to 5 million vectors

| Engine | Version | Index | Latency evidence | .NET | Insert and delete | Crash safety | Local encryption | Verdict |
|---|---|---|---|---|---|---|---|---|
| SQLite + sqlite-vec | 0.1.9 stable, 2026-03-31 [1] | Brute force over `vec0`, with `float`, `int8`, `bit` columns and partition keys [2] [3]. ANN (DiskANN, IVF) only in 0.1.10 alpha, DDL reported failing [4] | 1M x 128: 33 to 35 ms [5]. 1M x 512: 395 ms [6]. 1M x 1024: 590 ms flat, 101 ms with rescore [4]. No published 1M x 768 figure | `Microsoft.Data.Sqlite` + `LoadExtension`. NuGet package stuck at 0.1.7-alpha.2.1, ship `vec0.dll` yourself [7] | Plain SQL rows, real deletes | SQLite WAL | Through an encrypted SQLite build | Primary |
| USearch | `Cloud.Unum.USearch` 2.26.6 | HNSW, in memory or memory-mapped | Sub-ms to low ms at 1M (vendor) | Official C# | Add and remove, file saved whole | Rebuildable sidecar | None, encrypt the file yourself | Fallback at 5M |
| LanceDB | n/a | IVF-PQ, HNSW | Fast | No official .NET SDK | Delete writes a new table version. Old data stays until `optimize()` [8] [9] | Versioned files | None locally [10] [11] | Rejected |
| DuckDB vss | DuckDB 2.0 | HNSW | Fast | `DuckDB.NET` | HNSW persistence is behind `hnsw_enable_experimental_persistence` and index RAM is outside `memory_limit` [12]. Corruption reports [13] | Experimental | AES-256-GCM database encryption [14] | Rejected |
| SQLite `vec1` | Announced 2026-02-26 [15] | Brute force, float32 only, no partition keys | n/a | Same as SQLite | SQL | SQLite | Same | Watch |
| sqliteai sqlite-vector | n/a | Quantized scan | Vendor claims a few ms at 1M x 768 [16] | Extension | SQL | SQLite | Same | Rejected, licence already changed once [17] |

Microsoft Recall itself stores a SQLite database with an FTS table (`ukg.db`), one JPEG per snapshot in `ImageStore\`, and DiskANN graphs inside SQLite files (`SemanticTextStore.sidb`, `SemanticImageStore.sidb`) [18] [19] [20].

### 4.2 Search design on sqlite-vec

1. Stage 1: scan a `bit[768]` column made with `vec_quantize_binary()`. 96 bytes per vector: 96 MB at 1M, 480 MB at 5M [3]. Use a month partition key so a time-ranged query scans only the months it needs.
2. Stage 2: rerank the top few hundred candidates with int8 or float32 vectors. Measured quality loss against full float search: binary alone 6.50 nDCG@10 points, binary with rescore 0.93, int8 with rescore 0.09 [21].
3. Fuse with FTS5 results by reciprocal rank fusion.

Do not ship the alpha ANN indexes. sqlite-vec had no stable release between 0.1.6 (2024-11-20) and 0.1.7 (2026-03-17) [1], so keep the vector index behind an interface and keep float32 vectors as the source of truth. If 5M vectors prove too slow, build a USearch HNSW sidecar from those vectors. It is a cache and can be rebuilt after a crash.

### 4.3 Encryption at rest

| Option | Status | Cost | Source |
|---|---|---|---|
| `SQLitePCLRaw.bundle_e_sqlcipher` | Removed in SQLitePCLRaw 3.0. Last free build 2.1.11, March 2025 | n/a | [22] |
| SQLCipher Community | Source is free, official binaries and support are commercial | Paid binaries | [23] |
| SQLite3MultipleCiphers, `SQLite3MC.PCLRaw.bundle` 2.4.0, 2026-07-28, SQLite 3.53.4 | Free. Use with `Microsoft.Data.Sqlite.Core` and `Password=` in the connection string. The package notes .NET 10 as the minimum for `Password`. Default cipher ChaCha20-Poly1305 | Free | [24] [25] [26] |
| SQLite SEE | Official, paid, cannot be redistributed in an open-source project | Paid | [27] |
| LanceDB | No local encryption | n/a | [10] |
| DuckDB | Built in, AES-256-GCM | Free | [14] |

sqlite-vec under SQLite3MultipleCiphers is unverified. Encryption sits at the pager level, below SQL, so the extension should not notice. The sqlite3mc FAQ says to set the key before loading extensions and never to mix two SQLite builds in one process [28]. This needs a one-hour spike test.

Key storage:

| Mechanism | What it gives | Limit | Source |
|---|---|---|---|
| DPAPI `ProtectedData`, CurrentUser | Simple wrap of the data key | Any process running as the same user can unwrap | [29] |
| CNG Microsoft Platform Crypto Provider (TPM) | Non-exportable key. Use RSA, ECDSA has a known bug on this provider | Same-user processes can ask the TPM to use the key unless a UI policy is set | [30] |
| Windows Hello `KeyCredentialManager` | Hello-gated key. No unwrap API, so sign a fixed challenge with `RequestSignAsync` and derive an AES key from the signature | Any same-user process can open the same credential name, but the user sees a Hello prompt | [31] [32] |
| `UserConsentVerifier` | Yes or no prompt, no key material | A patched binary can skip it | [33] |
| VBS enclave | Key use isolated from the user session | Needs build 26100.2314+, a C/C++ enclave DLL, and production signing through Azure Artifact Signing, which individuals can get only in the US and Canada | [34] [35] [36] |

Recall binds its keys to the TPM and Windows Hello Enhanced Sign-in Security and uses them inside a VBS enclave, with its service running as a protected process [37]. Even so, a same-user process read decrypted data from the UI host `AIXHost.exe` after Hello authentication, and Microsoft called that by design [38] [39].

Design: a random 256-bit data key encrypts the databases. A TPM-held RSA key wraps it. Windows Hello gates the unwrap, with a session timeout. On machines without TPM or Hello, fall back to DPAPI CurrentUser and say so in the UI. This stops disk theft and other user accounts. It does not stop malware running as the same user, and the documentation must say that. No VBS enclave in v1.

Unverified: `KeyCredentialManager` behaviour in an unpackaged app.

### 4.4 Thumbnails and blobs

- Off by default, or on with a GB cap. Vectors and text alone answer "when did I see X". Thumbnails answer "show me".
- Store in a separate encrypted SQLite file per month (same key scheme). A monthly file makes retention a file delete and keeps the main database small.
- Format: 1280-wide WebP at quality 75 to 80, or AVIF. Published screenshot sizes: 41 to 245 KB for WebP, 46 to 224 KB for AVIF, against 103 to 618 KB for JPEG [40] [41] [42]. One test found AVIF ringing around small glyphs at high compression [43]. Test legibility before choosing.
- Purge: delete rows, `PRAGMA secure_delete=ON`, checkpoint the WAL, then `VACUUM` [44]. On an encrypted database leftover pages are ciphertext. "Purge everything" also destroys the wrapped data key, which makes every remaining byte unreadable.

### 4.5 Storage per day

Assumptions: 8 active hours, one capture per 5 s, so 5,760 captures. De-duplication keeps 20 to 50%, so 1,152 to 2,880 rows per day.

| Item | Per row | Per day | Per 30 days |
|---|---|---|---|
| float32 768-d vector | 3,072 B | 3.5 to 8.8 MB | 106 to 265 MB |
| int8 768-d vector | 768 B | 0.9 to 2.2 MB | 27 to 66 MB |
| binary vector | 96 B | 0.1 to 0.3 MB | 3 to 8 MB |
| OCR and accessibility text + FTS5 | 1 to 2 KB | 1.2 to 5.8 MB | 35 to 173 MB |
| Vectors + text total | n/a | 2 to 8 MB typical | 0.06 to 0.25 GB |
| 1280-wide thumbnails | 40 to 100 KB | 46 to 288 MB | 1.4 to 8.6 GB |
| Full-resolution frames | 150 to 300 KB | 173 to 864 MB | 5.2 to 26 GB |

Thumbnail sizes are estimates from published screenshot benchmarks and are unverified for this workload. For scale: Recall defaults to 25, 75 or 150 GB by disk size [45], screenpipe reports 5 to 30 GB per month [46] [47], Windrecorder 10 to 20 GB per month of video [48].

### 4.6 Recommendation

Use SQLite in WAL mode with FTS5 and sqlite-vec 0.1.9, brute-force two-stage search, month partitions. Reason: one file format with real deletes, crash safety, keyword and vector search in one transaction, first-class .NET support, and it is what Recall chose. Encrypt with SQLite3MultipleCiphers, wrap the key with the TPM behind Windows Hello, fall back to DPAPI. Keep thumbnails in separate monthly encrypted files, off by default.

### Sources for section 4

1. https://github.com/asg017/sqlite-vec/releases
2. https://alexgarcia.xyz/sqlite-vec/features/vec0.html
3. https://alexgarcia.xyz/sqlite-vec/guides/binary-quant.html
4. https://github.com/basicmachines-co/basic-memory/issues/1996
5. https://alexgarcia.xyz/blog/2024/sqlite-vec-stable-release/index.html
6. https://forum.golangbridge.org/t/benchmarking-a-pure-go-vector-index-against-hnswlib-and-sqlite-vec-every-number-i-got-wrong-first/42173
7. https://www.nuget.org/packages/sqlite-vec/
8. https://docs.lancedb.com/tables/update
9. https://github.com/lance-format/lance/discussions/5036
10. https://docs.lancedb.com/storage/configuration
11. https://docs.lancedb.com/enterprise/security
12. https://duckdb.org/docs/2.0/core_extensions/vss
13. https://github.com/duckdb/duckdb-vss/issues/81
14. https://duckdb.org/2025/11/19/encryption-in-duckdb
15. https://sqlite.org/vec1.html
16. https://github.com/sqliteai/sqlite-vector/blob/main/API.md
17. https://news.ycombinator.com/item?id=45347619
18. https://securelist.com/forensic-artifacts-in-windows-11/117680/
19. https://cybercx.com.au/blog/forensic-applications-of-microsoft-recall/
20. https://github.com/xaitax/TotalRecall
21. https://arxiv.org/pdf/2606.22778
22. https://github.com/ericsink/SQLitePCL.raw/blob/main/v3.md
23. https://www.zetetic.net/sqlcipher/license/
24. https://www.nuget.org/packages/SQLite3MC.PCLRaw.bundle
25. https://github.com/utelle/SQLite3MultipleCiphers-NuGet
26. https://utelle.github.io/SQLite3MultipleCiphers/
27. https://github.com/ericsink/SQLitePCL.raw/wiki/SQLite-encryption-options-for-use-with-SQLitePCLRaw
28. https://utelle.github.io/SQLite3MultipleCiphers/docs/faq/faq_overview/
29. https://comcomponent.com/en/blog/2026/03/16/000-windows-app-secret-storage-best-practices-dpapi/
30. https://github.com/dotnet/runtime/issues/24778
31. https://learn.microsoft.com/en-us/answers/questions/5887849/key-credential-manager-secure-storage-in-winui
32. https://clement.notin.org/blog/2019/12/17/When-Windows-Hello-fails-at-securely-authenticating-users-and-protecting-credentials/
33. https://learn.microsoft.com/en-us/windows/apps/develop/security/windows-hello-login
34. https://learn.microsoft.com/en-us/windows/win32/trusted-execution/vbs-enclaves
35. https://learn.microsoft.com/en-us/windows/win32/trusted-execution/vbs-enclaves-dev-guide
36. https://learn.microsoft.com/en-us/azure/artifact-signing/quickstart
37. https://blogs.windows.com/windowsexperience/2024/09/27/update-on-recall-security-and-privacy-architecture/
38. https://www.itnews.com.au/news/microsoft-says-new-windows-recall-bypass-isnt-a-vulnerability-624918
39. https://www.windowscentral.com/microsoft/windows-11/why-this-windows-recall-feature-raises-real-security-questions
40. https://allscreenshots.com/blog/png-jpeg-webp-avif-jpeg-xl-screenshots
41. https://pixotter.com/blog/webp-vs-avif/
42. https://www.imagecrush.io/blog/image-format-comparison-webp-avif-heic-jpeg-png
43. https://imagic-ai.com/blog/image-compression-formats-2026
44. https://sqlite.org/pragma.html#pragma_secure_delete
45. https://learn.microsoft.com/en-us/windows/client-management/manage-recall
46. https://docs.screenpipe.com/architecture
47. https://github.com/screenpipe/screenpipe
48. https://github.com/yuka-friends/Windrecorder

## 5. Process architecture

### 5.1 Host model

| Option | Can capture the screen | Starts at logon | Verdict |
|---|---|---|---|
| Windows service (LocalSystem or a service account) | No. Services run in session 0 with no access to the user's desktop [V1] | Yes | Rejected |
| Packaged background task | No long-running capture loop, lifetime managed by the system | Trigger based | Rejected |
| Per-user process in the interactive session | Yes | Yes, through `windows.startupTask` [T1] | Chosen |

Everything runs as the logged-on user at medium integrity. Nothing needs administrator rights after install.

### 5.2 Process split

| Process | Lifetime | Tech | Role |
|---|---|---|---|
| `agent` | Logon to logoff | .NET 10, Native AOT [N9], no XAML | Capture loop, guardrails, de-duplication, audio capture and VAD, global hotkey, tray icon through `Shell_NotifyIcon`, job queue, database writer, IPC server, resource governance |
| `worker` | On demand, exits when the queue is empty and idle | .NET 10 + ONNX Runtime | OCR, embedding, ASR. Holds the NPU or GPU. Killed and restarted on a driver fault without losing the agent |
| `ui` | On demand | WinUI 3, Windows App SDK 2.x | Quick-search launcher, timeline, settings. Holds no key. Asks the agent for results |

Reasons for the split: WinUI 3 idle memory is reported between tens of MB and about 155 MB [N10] [N11] [N12] (anecdotal), which is too much for an always-on process. Inference runtimes leak and crash, so they belong in a disposable process. Recall's weak point was decrypted data in its UI host (section 4), so the UI gets rendered results only and only after the Hello gate.

Versions: Windows App SDK 2.x (2.6.5 of 2026-11-02 is current, the 2.0 line is supported to 2027-04-29) [N1] [N2] [N3]. .NET 10 LTS, supported to 2028-11-14 [N4] [N5]. Native AOT with CsWinRT has trimming rules to follow [N7] [N8]. The minimum .NET version for Windows App SDK 2.x is unverified.

### 5.3 Startup, hotkey, tray

- Startup: `windows.startupTask` in the package manifest. A packaged desktop app's task is enabled without a consent dialog and the user can turn it off in Settings [T1].
- Hotkey: `RegisterHotKey` with `MOD_NOREPEAT` in the agent [K3]. On press, the agent starts or signals the UI and calls `AllowSetForegroundWindow` for the UI's process id so the launcher can take focus [K6]. PowerToys Command Palette uses the same pattern and keeps its window warm [K4] [K5].
- Tray: raw `Shell_NotifyIcon` in the agent. H.NotifyIcon.WinUI 2.4.1 is the option if the tray moves into the UI process [K1] [K2].

### 5.4 Packaging

| Mode | Startup task | Borderless WGC | Notes |
|---|---|---|---|
| MSIX | Yes | Yes, with the capability | Needs a signing certificate. Clean uninstall |
| Unpackaged + external-location package identity [T2] | Yes | Yes | Normal installer plus a sparse package for identity. Still needs signing |
| Unpackaged, no identity | Registry Run key | Unverified | Developer mode. Desktop Duplication only |

Recommendation: MSIX for releases, unpackaged for development.

### 5.5 Staying out of the way

| Signal | API | Action |
|---|---|---|
| Fullscreen game, presentation, busy | `SHQueryUserNotificationState`: `QUNS_RUNNING_D3D_FULL_SCREEN`, `QUNS_PRESENTATION_MODE`, `QUNS_BUSY` [E1] [E2] | Pause capture (user option) and stop the worker |
| Always | EcoQoS through `PROCESS_POWER_THROTTLING_EXECUTION_SPEED` [E3] [E4] | Agent and worker run on efficiency cores at low frequency |
| Always | `PROCESS_MODE_BACKGROUND_BEGIN` [E5] | Low I/O and memory priority for the worker |
| Heavy foreground load (build, render) | CPU sets, lowest `EfficiencyClass` [E6] [E7]. Sample total CPU and GPU load | Defer embedding. Keep capturing, spill downscaled frames to an encrypted queue on disk with a size cap |
| Battery | `GetSystemPowerStatus`, battery saver flag [E8] | Pause inference |
| Idle | Last-input time | Drain the queue |

NPU work is scheduled, not immediate. The agent captures and queues. The worker drains in batches when the machine is idle or lightly loaded. Search stays correct because text (accessibility and OCR) is cheap and can be indexed at once, while vectors arrive later.

### 5.6 Recommendation

Three per-user processes (`agent`, `worker`, `ui`), no service, MSIX with a startup task, .NET 10 and Windows App SDK 2.x. Reason: only an interactive-session process can capture, the always-on part must be small, and the inference part must be disposable.

### Sources for sections 3 and 5

Capture
- [W1] https://learn.microsoft.com/en-us/windows/apps/develop/media-authoring-processing/screen-capture
- [W2] https://learn.microsoft.com/en-us/windows/win32/api/windows.graphics.capture.interop/nn-windows-graphics-capture-interop-igraphicscaptureiteminterop
- [S2] https://learn.microsoft.com/en-us/uwp/api/windows.graphics.capture.graphicscapturesession.isborderrequired
- [S3] https://learn.microsoft.com/en-us/uwp/api/windows.graphics.capture.graphicscapturesession.minupdateinterval
- [S4] https://learn.microsoft.com/en-us/uwp/api/windows.graphics.capture.graphicscapturesession.dirtyregionmode
- [S5] https://learn.microsoft.com/en-us/uwp/api/windows.graphics.capture.graphicscapturesession.includesecondarywindows
- [S6] https://learn.microsoft.com/en-us/uwp/api/windows.graphics.capture.graphicscaptureaccess.requestaccessasync
- [C1] https://learn.microsoft.com/en-us/windows/apps/package-and-deploy/app-capability-declarations
- [D1] https://learn.microsoft.com/en-us/windows/win32/direct3ddxgi/desktop-dup-api
- [D2] https://learn.microsoft.com/en-us/windows/win32/api/dxgi1_2/nf-dxgi1_2-idxgioutputduplication-acquirenextframe
- [D3] https://learn.microsoft.com/en-us/windows/win32/api/dxgi1_2/nf-dxgi1_2-idxgioutput1-duplicateoutput
- [D4] https://learn.microsoft.com/en-us/windows/win32/api/dxgi1_5/nf-dxgi1_5-idxgioutput5-duplicateoutput1
- [A1] https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-setwindowdisplayaffinity
- [A2] https://www.ioactive.com/the-drm-flag-that-isnt-drm/
- [A3] https://blogs.windows.com/windowsdeveloper/2019/09/16/new-ways-to-do-screen-capture/
- [V1] https://learn.microsoft.com/en-us/windows/win32/services/interactive-services
- [P3] https://docs.screenpipe.com/architecture
- [P4] https://screenpipe.com/changelog
- [P5] https://raw.githubusercontent.com/openrecall/openrecall/main/openrecall/screenshot.py
- [P6] https://raw.githubusercontent.com/yuka-friends/Windrecorder/main/README.md

Audio
- [U1] https://learn.microsoft.com/en-us/windows/win32/coreaudio/loopback-recording
- [U2] https://learn.microsoft.com/en-us/windows/win32/api/audioclientactivationparams/ne-audioclientactivationparams-process_loopback_mode
- [U3] https://learn.microsoft.com/en-us/windows/win32/api/audioclientactivationparams/ns-audioclientactivationparams-audioclient_process_loopback_params
- [U4] https://learn.microsoft.com/en-us/windows/win32/api/mmdeviceapi/nf-mmdeviceapi-activateaudiointerfaceasync
- [U5] https://learn.microsoft.com/en-us/samples/microsoft/windows-classic-samples/applicationloopbackaudio-sample/
- [U6] https://support.microsoft.com/en-us/windows/windows-camera-microphone-and-privacy-a83257bc-e990-d54a-d212-b5e41beba857
- [L1] https://naudio.github.io/NAudio/docs/WasapiRecorder.html
- [L2] https://github.com/naudio/NAudio/issues/878
- [L3] https://github.com/filoe/cscore/blob/master/CSCore/SoundIn/WasapiLoopbackCapture.cs
- [L4] https://github.com/snakers4/silero-vad/releases
- [L5] https://github.com/snakers4/silero-vad/wiki/Version-history-and-Available-Models

Privacy
- [R1] https://learn.microsoft.com/en-us/windows/client-management/manage-recall
- [R2] https://learn.microsoft.com/en-us/windows/apps/develop/windows-integration/recall/recall-web-browsers
- [R3] https://support.microsoft.com/en-us/windows/privacy-and-control-over-your-recall-experience-d404f672-7647-41e5-886c-a3c59680af15
- [R5] https://learn.microsoft.com/en-us/windows/apps/develop/windows-integration/recall/recall-content-info-labels
- [B1] https://issues.chromium.org/issues/41483360
- [B2] https://chromium.googlesource.com/chromium/src/+/a0bed883c074617e8e8ddfa379ab5d563e6ed2da%5E!/
- [B3] https://brave.com/privacy-updates/35-block-recall/
- [B4] https://github.com/brave/brave-browser/issues/46284
- [B5] https://www.bleepingcomputer.com/news/security/brave-blocks-windows-recall-from-screenshotting-your-browsing-activity/
- [B6] https://signal.org/blog/signal-doesnt-recall/
- [G1] https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-setwineventhook
- [G2] https://learn.microsoft.com/en-us/windows/win32/api/winbase/nf-winbase-queryfullprocessimagenamew
- [G3] https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-getwindowdisplayaffinity
- [G4] https://learn.microsoft.com/en-us/windows/win32/api/wtsapi32/nf-wtsapi32-wtsregistersessionnotification
- [G5] https://learn.microsoft.com/en-us/windows/win32/termserv/wm-wtssession-change

Architecture
- [T1] https://learn.microsoft.com/en-us/uwp/api/windows.applicationmodel.startuptask
- [T2] https://learn.microsoft.com/en-us/windows/apps/desktop/modernize/grant-identity-to-nonpackaged-apps
- [N1] https://learn.microsoft.com/en-us/windows/apps/windows-app-sdk/release-notes/windows-app-sdk-2-0
- [N2] https://learn.microsoft.com/en-us/windows/apps/windows-app-sdk/downloads
- [N3] https://learn.microsoft.com/en-us/windows/apps/windows-app-sdk/release-channels
- [N4] https://github.com/dotnet/core/blob/main/release-notes/10.0/README.md
- [N5] https://learn.microsoft.com/en-us/dotnet/core/releases-and-support
- [N7] https://github.com/microsoft/CsWinRT/blob/master/docs/aot-trimming.md
- [N8] https://github.com/dotnet/sdk/issues/53387
- [N9] https://learn.microsoft.com/en-us/dotnet/core/deploying/native-aot/
- [N10] https://github.com/microsoft/microsoft-ui-xaml/issues/8736
- [N11] https://github.com/microsoft/microsoft-ui-xaml/discussions/11096
- [N12] https://www.ctco.blog/posts/winui-vs-wpf-2026-practical-comparison/
- [K1] https://www.nuget.org/packages/H.NotifyIcon.WinUI
- [K2] https://github.com/HavenDV/H.NotifyIcon
- [K3] https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-registerhotkey
- [K4] https://github.com/microsoft/PowerToys/pull/38250
- [K5] https://learn.microsoft.com/en-us/windows/powertoys/command-palette/settings
- [K6] https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-setforegroundwindow
- [E1] https://learn.microsoft.com/en-us/windows/win32/api/shellapi/ne-shellapi-query_user_notification_state
- [E2] https://learn.microsoft.com/en-us/windows/win32/api/shellapi/nf-shellapi-shqueryusernotificationstate
- [E3] https://learn.microsoft.com/en-us/windows/win32/procthread/quality-of-service
- [E4] https://learn.microsoft.com/en-us/windows/win32/api/processthreadsapi/ns-processthreadsapi-process_power_throttling_state
- [E5] https://learn.microsoft.com/en-us/windows/win32/api/processthreadsapi/nf-processthreadsapi-setpriorityclass
- [E6] https://learn.microsoft.com/en-us/windows/win32/procthread/cpu-sets
- [E7] https://learn.microsoft.com/en-us/windows/win32/api/winnt/ns-winnt-system_cpu_set_information
- [E8] https://learn.microsoft.com/en-us/windows/win32/api/winbase/ns-winbase-system_power_status

## 6. Prior art

| Project | Platform, stack | Capture | Search | Security at rest | Status, licence |
|---|---|---|---|---|---|
| Microsoft Recall | Windows 11 Copilot+ only: 40-TOPS NPU, 16 GB RAM, Secured-core [1] | Periodic snapshots, app and site exclusions, sensitive-information filter | OCR text in SQLite FTS, DiskANN image and text indexes [2] [3] | TPM keys bound to Windows Hello Enhanced Sign-in Security, used in a VBS enclave, service runs as a protected process [4] | GA 2025-04-25, opt-in [5] [6]. Closed. Cannot run on the 265K |
| screenpipe | Rust + Tauri, Windows, macOS, Linux | Event driven, accessibility tree first, OCR fallback, audio with Whisper | SQLite FTS5. No vector search in its architecture doc. REST API on localhost:3030 and an MCP server [7] | Local files | Active. Now a source-available commercial licence [8]. 5 to 20% CPU, 0.5 to 3 GB RAM [7] |
| OpenRecall | Python, cross-platform | Timer, SSIM de-duplication | OCR + text embedding | Plaintext | AGPL-3.0. Stalled since about 2025-09 [9] |
| Windrecorder | Python, Windows | Video recording, skips static screens, indexes when idle | Windows OCR text, optional image embedding extension | Local files | GPL-2.0, active [10] |
| Rewind / Limitless | macOS app, then a pendant | Screen + audio | Cloud-assisted | n/a | Meta bought Limitless on 2025-12-05. Rewind capture ended 2025-12-19 [11] |
| Pieces LTM-2 | Desktop app | Workstream capture | On-device and cloud models | Local | Commercial [12] |
| rem | macOS, Swift | Timer | OCR text | Plaintext | MIT, dormant [13] |
| RewindOS, linux_recall | Linux | Timer | OCR, embeddings | Varies | Small community projects [14] [15] |

Recall's security record after the redesign:

- The sensitive-information filter missed card numbers and passwords in tests (2025-08) [16].
- TotalRecall Reloaded (March 2026) read decrypted snapshots from the UI host process after Hello authentication, and some metadata without it. Microsoft called it by design [3] [17] [18].
- Signal, Brave and AdGuard block Recall from their side [19].
- Apps opt out with `SetWindowDisplayAffinity(WDA_EXCLUDEFROMCAPTURE)` [20].

Copy:

- Opt-in at first run. Nothing is captured before the user says yes.
- A GB cap plus retention in days, both visible in settings (Recall).
- App deny list, pause control, honour `WDA_EXCLUDEFROMCAPTURE` (Recall).
- Windows Hello gate with a timeout (Recall).
- Event-driven capture and accessibility text first (screenpipe).
- Index when idle, skip static screens (Windrecorder).
- Hash and SSIM de-duplication (OpenRecall).
- SQLite with FTS as the base store (Recall, screenpipe, Windrecorder).

Avoid:

- Plaintext stores (OpenRecall, rem).
- Decrypted data held in the UI process (Recall).
- A licence change after people adopt the project (screenpipe). Pick the licence now and state it.
- Trusting a regex filter to catch secrets (Recall).
- Unbounded disk and RAM use (screenpipe reports).
- A localhost HTTP API on by default (screenpipe). Any local process and any web page that can reach localhost becomes a client.
- A hard dependency on one maintainer's library with no fallback (sqlite-vec's 16-month gap).

Recommendation: BetterRecall's gap in the market is real. No open-source Windows recorder combines encrypted storage, visual embeddings and NPU inference on non-Copilot+ hardware. Copy Recall's controls and screenpipe's capture strategy, and keep the security claims modest: disk theft and other users are stopped, same-user malware is not.

### Sources for section 6

1. https://learn.microsoft.com/en-us/windows/client-management/manage-recall
2. https://securelist.com/forensic-artifacts-in-windows-11/117680/
3. https://github.com/xaitax/TotalRecall
4. https://blogs.windows.com/windowsexperience/2024/09/27/update-on-recall-security-and-privacy-architecture/
5. https://blogs.windows.com/windowsexperience/2025/04/25/copilot-pcs-are-the-most-performant-windows-pcs-ever-built-now-with-more-ai-features-that-empower-you-every-day/
6. https://borncity.com/win/2025/04/26/microsoft-rolls-out-recall-for-windows-11-24h2-on-copilot-pcs/
7. https://docs.screenpipe.com/architecture
8. https://github.com/screenpipe/screenpipe
9. https://github.com/openrecall/openrecall
10. https://github.com/yuka-friends/Windrecorder
11. https://winbuzzer.com/2025/12/05/meta-acquires-ai-wearables-startup-limitless-kills-pendant-sales-and-sunsets-rewind-app-xcxwbn/
12. https://pieces.app/updates/ltm-2-workstream-activity-our-biggest-update-yet
13. https://github.com/jasonjmcghee/rem
14. https://github.com/jaypopat/rewindos
15. https://github.com/junyixu/linux_recall
16. https://www.theregister.com/security/2025/08/01/microsoft-recall-can-still-nab-credit-cards-passwords-info/971447
17. https://www.govinfosecurity.com/microsoft-recall-again-spills-secrets-a-31083
18. https://www.geekwire.com/2026/one-year-after-its-rocky-launch-microsofts-windows-recall-still-raises-security-red-flags/
19. https://www.theregister.com/on-prem/2025/07/23/brave-browser-blocks-privacy-busting-microsoft-recall/1171293
20. https://learn.microsoft.com/en-us/windows/apps/develop/windows-integration/recall/

## 7. Optional IPC and MCP

This is a design proposal, not a research finding. It follows the named-pipe conventions of the user's ESP32 PC Control System (line framing, `ok`/`error` envelope, verb style, error wording, client trust check) and changes what must change because the server is a per-user process, not a Windows service.

### 7.1 Threat model

A search result is a window into everything the user has seen. The pipe is therefore as sensitive as the database key.

| Actor | May connect | How it is kept out |
|---|---|---|
| Same user, medium or high integrity, same session | Yes, when `ipc.enabled` is true | n/a |
| Same user, low integrity or AppContainer (browser sandbox, packaged apps) | No | Mandatory label on the pipe at medium with no-write-up. No ACE for `ALL APPLICATION PACKAGES` |
| Other local users, services | No | DACL grants only the agent's user SID |
| Remote machines over SMB | No | `PIPE_REJECT_REMOTE_CLIENTS`, plus a deny ACE for `NETWORK` |
| Same user, other session (RDP, fast user switching) | No | Server compares `GetNamedPipeClientSessionId` with its own session |
| Fake server squatting the pipe name | n/a | `FILE_FLAG_FIRST_PIPE_INSTANCE` on the server, and the client verifies the server process |
| Same-user malware at medium integrity | Cannot be stopped by a DACL | Default off, optional client allow list, Hello-locked store, rate limit, audit log, tray indicator while a client is connected |

The last row is the honest limit. Turning IPC on widens what same-user malware can do from "steal an encrypted file" to "ask questions". The settings page must say so next to the switch.

### 7.2 Config keys

| Key | Default | Effect | Validation |
|---|---|---|---|
| `ipc.enabled` | `false` | When false the agent creates no pipe at all | bool |
| `ipc.pipeName` | `BetterRecall` | Base name. A `\\.\pipe\` or `\pipe\` prefix is stripped. Effective name is `<pipeName>.<user SID>` | 1 to 64 chars, `[A-Za-z0-9._-]` |
| `ipc.allowControl` | `false` | Enables `pause` and `resume` | bool |
| `ipc.allowPurge` | `false` | Enables `purge`. Needs `ipc.allowControl` | bool |
| `ipc.allowThumbnails` | `false` | Enables `thumb` | bool |
| `ipc.allowClients` | `[]` | Full image paths allowed to connect. Empty means any process that passes the checks in 7.4 | absolute paths |
| `ipc.requireUnlocked` | `true` | Data verbs fail while the store is locked. IPC can never trigger or bypass the Hello prompt | bool |
| `ipc.maxResults` | `50` | Upper bound for `limit` | 1 to 200 |
| `ipc.defaultResults` | `10` | `limit` when the request gives none | 1 to `ipc.maxResults` |
| `ipc.maxResponseBytes` | `262144` | Largest response line. Results are dropped from the end and `truncated` is set | 4096 to 1048576 |
| `ipc.maxSearchPerMinute` | `60` | Per connection | 1 to 600 |
| `ipc.maxPauseS` | `86400` | Longest `pause <seconds>` | 60 to 604800 |
| `ipc.idleTimeoutS` | `300` | Close a request-mode connection after this much silence | 10 to 3600 |
| `mcp.enabled` | `false` | Creates the MCP pipe (7.8) | bool |

The pipe name carries the user SID because pipe names are machine-wide, and two logged-on users would otherwise collide.

### 7.3 Pipe

- Name: `\\.\pipe\BetterRecall.<user SID>`, for example `\\.\pipe\BetterRecall.S-1-5-21-...-1001`.
- Byte mode, overlapped, 4 instances, `FILE_FLAG_FIRST_PIPE_INSTANCE` on the first, `PIPE_REJECT_REMOTE_CLIENTS` on all.
- Security descriptor (SDDL, `<SID>` is the agent's user): `O:<SID>D:P(D;;GA;;;NU)(A;;GRGW;;;<SID>)S:(ML;;NWNR;;;ME)`.
  - `D:P` protected DACL, nothing inherited.
  - Deny all to `NETWORK`.
  - Read and write to the user only. No ACE for SYSTEM or Administrators. An administrator can already read the user's memory, so an ACE would add nothing.
  - Mandatory label medium, no-write-up and no-read-up.
- Created at agent start when `ipc.enabled` is true. Toggling the setting creates or closes the pipe without a restart and drops live connections.

### 7.4 Trust checks

Server, on every connection, before reading a byte:

1. `GetNamedPipeClientProcessId` and `GetNamedPipeClientSessionId`. The session must equal the agent's session.
2. Open the client process with `PROCESS_QUERY_LIMITED_INFORMATION`, read its token: user SID equals the agent's SID, integrity level is medium or higher, token is not an AppContainer token.
3. `QueryFullProcessImageNameW`. If `ipc.allowClients` is not empty, the path must be in it (case-insensitive, after normalisation).
4. Any failure: write `{"ok":false,"error":"Client not allowed."}` and close. Record it in the audit log.

The server never impersonates the client.

Client (the bundled `betterrecall ipc` CLI and the MCP process do this, third parties should):

1. Connect with `TokenImpersonationLevel.Anonymous`, 1 s timeout.
2. `GetNamedPipeServerProcessId`, then `QueryFullProcessImageNameW`. The path must equal the installed agent path (package install location, or the `InstallPath` registry value under the app's HKCU key for unpackaged installs). The server's session must equal the client's.
3. Optional: the agent binary's Authenticode signer equals the client's own signer.
4. Failure messages: `err BetterRecall not running` (no pipe, or connect timeout), `err BetterRecall not trusted` (path, session or signer mismatch).

CLI: `betterrecall ipc <words...>` sends one line, prints the response line. Exit code 0 on `ok:true`, 1 on `ok:false` or no response, 2 when the agent is unreachable or untrusted.

### 7.5 Framing

- UTF-8. One request per `\n`. `\r` and a leading BOM (U+FEFF) are stripped.
- One compact JSON line back per request, ending in `\n`.
- Request line limit 4096 bytes. Over the limit: `{"ok":false,"error":"Line exceeds 4096 bytes."}` and the server closes the connection.
- Success: `{"ok":true,"data":...}`. Failure: `{"ok":false,"error":"<message>"}`.
- Verbs are case-insensitive. Arguments are split on spaces, except that everything after `search ` is taken whole.
- The command processor never throws. An unexpected exception becomes `{"ok":false,"error":"Internal error."}` and the detail goes to the log.
- Timestamps are ISO 8601 UTC with `Z`. Ids are opaque strings.

### 7.6 Verbs

Read-only verbs (always available when the pipe exists):

| Verb | Request | `data` on success |
|---|---|---|
| `ping` | `ping` | `{"pong":true}` |
| `version` | `version` | `{"app":"0.1.0","protocol":1}` |
| `help` | `help` | `{"verbs":["ping","version",...]}`. Lists only verbs enabled by config |
| `status` | `status` | `{"capturing":true,"paused":false,"pausedUntil":null,"pauseReason":null,"locked":false,"audio":{"system":false,"mic":false},"queue":{"pending":12},"device":"GPU","store":{"rows":184223,"bytes":412334080,"oldest":"2026-08-01T02:11:09Z","newest":"2026-11-10T07:40:12Z"},"retentionDays":90}` |
| `search` | `search <free text>` or `search <json>` (below) | `{"count":3,"truncated":false,"tookMs":41,"results":[<result>...]}` |
| `get` | `get <id>` | One `<result>` with `"text"` (full stored text, cut to fit `ipc.maxResponseBytes`) in place of `"snippet"` |
| `thumb` | `thumb <id>` | `{"id":"...","mime":"image/webp","width":1280,"height":720,"base64":"..."}`. Needs `ipc.allowThumbnails` |
| `subscribe` | `subscribe` or `subscribe captures` | Ack, then a stream (7.7) |

`search` JSON form. Every field except `q` is optional:

```json
{"q":"invoice from Acme","limit":10,"from":"2026-11-01T00:00:00Z","to":"2026-11-10T00:00:00Z","app":"chrome.exe","kind":["screen","audio"],"mode":"hybrid"}
```

| Field | Type | Default | Rule |
|---|---|---|---|
| `q` | string | required | 1 to 1024 chars after trim |
| `limit` | int | `ipc.defaultResults` | 1 to `ipc.maxResults`, values above are clamped |
| `from`, `to` | ISO 8601 | none | `from` must be earlier than `to` |
| `app` | string | none | Process image name, exact, case-insensitive |
| `kind` | array | both | `screen`, `audio` |
| `mode` | string | `hybrid` | `hybrid`, `text`, `vector` |

A request whose first non-space character after `search` is `{` is parsed as JSON. Anything else is free text with all defaults.

`<result>` shape:

```json
{"id":"f_01JC8Z...","ts":"2026-11-09T14:03:27Z","kind":"screen","app":"chrome.exe","title":"Acme invoice 4471 - Gmail","score":0.83,"snippet":"...Invoice 4471 due 2026-11-30...","hasThumb":false}
```

`snippet` is at most 300 characters. `score` is the fused score from 0 to 1. Audio results add `"durationS"`.

State-changing verbs (need `ipc.allowControl`):

| Verb | Request | `data` | Notes |
|---|---|---|---|
| `pause` | `pause` or `pause <seconds>` | `null` | Without seconds, pauses until `resume`. Seconds from 1 to `ipc.maxPauseS` |
| `resume` | `resume` | `null` | Clears an IPC or user pause. Does not override a guardrail pause (lock, deny list) |
| `purge` | `purge last <seconds>`, `purge range <from> <to>`, `purge app <name>` | `{"confirm":"<token>","rows":1234}` | Step 1 of 2. Also needs `ipc.allowPurge` |
| `purge confirm` | `purge confirm <token>` | `{"purged":1234}` | Token is single use, bound to the connection, valid 30 s |

There is no `purge all` over IPC, no verb to change settings, no verb to unlock the store and no verb to change deny lists. Those stay in the UI behind Windows Hello.

Error strings:

| Condition | `error` |
|---|---|
| Unknown verb | `Unknown command '<x>'.` |
| Feature off | `Control is off (config ipc.allowControl).`, `Purge is off (config ipc.allowPurge).`, `Thumbnails are off (config ipc.allowThumbnails).` |
| Store locked | `Store is locked.` |
| Bad arguments | `Usage: search <text> \| search <json>`, `Usage: get <id>`, `Usage: pause [seconds]`, `Usage: purge last <seconds> \| purge range <from> <to> \| purge app <name> \| purge confirm <token>`, `Usage: subscribe [captures]` |
| Bad JSON | `Invalid JSON.` |
| Field out of range | `Invalid <field>.` |
| Unknown id | `Not found.` |
| Rate limit | `Rate limit exceeded.` |
| Bad or expired purge token | `Invalid or expired token.` |
| Trust check failed | `Client not allowed.` |
| Oversized line | `Line exceeds 4096 bytes.` |
| Anything else | `Internal error.` |

### 7.7 Subscribe

- `subscribe` acks with `{"ok":true,"data":{"subscribed":true}}`, then sends `{"ok":true,"data":<status>}` on every state change (pause, resume, lock, unlock, device change) and at least every 60 s.
- `subscribe captures` acks with `{"ok":true,"data":{"subscribed":"captures"}}`, then sends one line per stored capture: `{"ok":true,"data":{"id":"...","ts":"...","kind":"screen","app":"chrome.exe"}}`. Metadata only: no title, no text. Fails with `Store is locked.` when locked.
- A bad target returns `Usage: subscribe [captures]` and the connection stays in request mode.
- After a successful subscribe the connection is a one-way stream. The server ignores further input and the client closes the pipe to stop. A slow client that lets 64 messages queue is disconnected.

### 7.8 MCP

- A separate executable, `betterrecall-mcp.exe`, speaks MCP over stdio. The MCP client (Claude Desktop, an IDE) launches it. It holds no key and opens no database. It is a pipe client of the agent and runs the client trust check from 7.4.
- It does not reuse the general pipe. When `mcp.enabled` is true the agent creates a second pipe, `\\.\pipe\BetterRecall.mcp.<user SID>`, with the same security descriptor, framing and command processor, and a fixed read-only verb set: `ping`, `version`, `status`, `search`, `get`. No `thumb`, `pause`, `purge` or `subscribe`. The `ipc.*` limits apply.
- Reason for a second endpoint: `mcp.enabled` and `ipc.enabled` stay independent switches, MCP can never reach a state-changing verb even when `ipc.allowControl` is on, and the audit log and tray indicator can tell an AI client from a script.
- MCP tools: `search_history` (maps to `search`, same fields), `get_capture` (maps to `get`), `recall_status` (maps to `status`). Tool results are text.
- No HTTP or SSE transport. A localhost port is reachable by every local process and by web pages, and has no DACL.
- An MCP client sends what it reads to its model, which for a cloud model leaves the machine. BetterRecall still makes zero outbound connections, but the settings page must state that turning MCP on lets the connected assistant send search results to its provider.

### 7.9 Audit

The agent appends one row per connection and per data verb to an audit table in the encrypted database: time, pipe (`ipc` or `mcp`), client process id and image path, verb, result count, SHA-256 of the query text. The UI shows it. The tray icon changes while any client is connected.

### 7.10 Recommendation

Ship the pipe as specified, off by default, with read-only verbs as the base set and each wider capability behind its own switch. Put MCP on its own read-only pipe behind `mcp.enabled`. Reason: the DACL, mandatory label and session check keep out remote, other-user and sandboxed processes, and the remaining exposure (same-user malware) is limited by default-off, the Hello lock, rate limits and a visible audit trail.

Unverified: that the mandatory-label SACL can be set on a pipe from managed code without `SeSecurityPrivilege` (label ACEs normally can be set by the owner, test it), and that `GetNamedPipeServerProcessId` returns the agent's process id for a Native AOT process using `NamedPipeServerStream`.

## 8. Open risks and decisions

Decisions for the user:

1. Licence. MIT or Apache 2.0 allows any reuse. GPL or AGPL blocks closed forks. Decide before the first commit. EmbeddingGemma 2, SigLIP 2, ONNX Runtime, OpenVINO and sqlite-vec all permit either.
2. Thumbnails. Off (text and vectors only, about 0.1 to 0.25 GB per month) or on with a GB cap (1.4 to 8.6 GB per month).
3. Audio in v1 or later. It adds Whisper, a second NPU workload and the microphone indicator.
4. Same-user malware. Accept the limit and document it, or plan a VBS enclave later (needs build 26100.2314+ and a signing route that individuals outside the US and Canada may not have).
5. Machines without TPM or Windows Hello. Allow a DPAPI fallback, or refuse to run.
6. Packaging and signing. MSIX needs a code-signing certificate. Without one, releases are unpackaged and lose the startup task and borderless WGC.
7. Private browsing. Ship "exclude this browser" plus best-effort title detection, or exclude all browsers by default until detection is proven.
8. Windows ML or plain ONNX Runtime. Depends on the packet-capture test below.
9. Minimum Windows build. Process loopback and borderless WGC need build 20348 or later, so Windows 11 only is the simple answer.
10. Vietnamese. If it is a first-class target, EmbeddingGemma 2 retrieval, OCR and ASR each need a Vietnamese test set before the model choice is final.

Tests to run on the 265K before design freezes, in order of risk:

| Test | Decides |
|---|---|
| EmbeddingGemma 2 vision tower compiles and matches the CPU reference (cosine 0.999 or better) on NPU 3720 and on the iGPU, with latency at 140 tokens | Primary model or SigLIP 2. NPU or GPU as default device |
| float16 NaN check for EmbeddingGemma 2 on each device | Precision variant to ship |
| sqlite-vec 0.1.9 loaded into a SQLite3MultipleCiphers build, key set first | Encryption design |
| 1M and 5M x 768 binary scan plus rescore latency | Whether the USearch sidecar is needed |
| Packet capture of the Windows ML bring-your-own-EP path | Windows ML or plain ONNX Runtime |
| OpenVINO plugin EP 1.8.1 and DirectML in one process with ORT 1.28.3 | Packaging of execution providers |
| Read `IS_PRIVATE` input scope or a UI Automation private-mode marker from another process, per browser | Private-window guardrail |
| `Get-WindowsCapability -Online \| Where-Object Name -Like 'Language.OCR*'` for vi-VN, and PP-OCRv5 on Vietnamese text with tone marks | OCR engine |
| Whisper large-v3-turbo on NPU under Windows: compile time, real-time factor, Vietnamese error rate against PhoWhisper | ASR model |
| WGC `IsBorderRequired=false` without package identity | Whether the unpackaged build can use the fallback capture path |
| Mandatory label on the pipe, and the server process id check, from Native AOT | IPC trust checks |
| `KeyCredentialManager` in an unpackaged app | Key gating in developer builds |
| Idle CPU, RAM and GPU use of the agent during a game and a full build | Governance thresholds |

Risks that no test removes:

- sqlite-vec has one maintainer and went 16 months without a stable release. The vector index stays behind an interface.
- EmbeddingGemma 2 is five weeks old. Community exports may change, and the ONNX and OpenVINO conversions are not from Google.
- DirectML is in sustained engineering. AMD NPU support depends on VitisAI becoming bundle-friendly, or stays on GPU.
- Compiled-model caches break on driver updates. The first run after an update recompiles for minutes.
- A regex filter will miss secrets. The UI must not promise that passwords are never stored.
- Windows App SDK 2.0 support ends 2027-04-29. Plan an SDK update in the first year.

