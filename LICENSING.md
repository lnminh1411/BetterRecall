# Licensing

BetterRecall uses two licences. Every source file starts with an `SPDX-License-Identifier` line that names its licence.

| Part | Folders | Licence | Text |
|---|---|---|---|
| Client side: WinUI app and the shared contracts it links | `src/BetterRecall.UI`, `src/BetterRecall.Contracts`, `tests/BetterRecall.UI.Tests` | MIT | `LICENSES/MIT.txt` |
| Backend: agent, worker, core, CLI, MCP server, their tests, scripts | everything else | AGPL-3.0-only | `LICENSE`, `LICENSES/AGPL-3.0.txt` |

`BetterRecall.Contracts` holds the IPC records, settings and pipe names. It is MIT because the UI compiles it in. The backend links it too, which MIT allows.

The UI talks to the agent only over a named pipe, so the two run as separate programs. You can reuse the UI and contracts under MIT terms. The backend stays under AGPL-3.0: if you change it and let people use it over a network, you must offer them your source.

## Models

Model weights are not in this repository. The converted models (OpenVINO IR and ONNX) are published separately on Hugging Face at [`lnminh1411/BetterRecall-models`](https://huggingface.co/lnminh1411/BetterRecall-models), each folder under its upstream licence:

| Model | Upstream | Licence |
|---|---|---|
| EmbeddingGemma 2 | `google/embeddinggemma-2` | Apache-2.0 |
| SigLIP 2 base patch16-224 | `google/siglip2-base-patch16-224` | Apache-2.0 |
| Whisper large-v3-turbo | `openai/whisper-large-v3-turbo` | MIT |
| NSFW classifier | `Falconsai/nsfw_image_detection` | Apache-2.0 |
| NSFW detector (NudeNet 320n ONNX) | `deepghs/nudenet_onnx` (ONNX of `notAI-tech/NudeNet`) | AGPL-3.0 |
| Silero VAD v6.2.3 | `snakers4/silero-vad` | MIT |
| PaddleOCR PP-OCRv6 small (detector, main recogniser) + PP-OCRv5 mobile script recognisers | `PaddlePaddle/PP-OCRv6_small_det_onnx`, `PaddlePaddle/PP-OCRv6_small_rec_onnx`, `PaddlePaddle/<script>_PP-OCRv5_mobile_rec_onnx` | Apache-2.0 |
| PII token classifier (names, addresses) | `Wismut/nym-pii-multilingual-small` (fine-tune of `jhu-clsp/mmBERT-small`) | MIT |

The NudeNet ONNX repackaging is tagged Apache-2.0, but the original NudeNet project is AGPL-3.0, so the detector is distributed under AGPL-3.0.

`Scripts/models/` can also rebuild them from the official upstream files.

## Third-party runtime pieces

- `src/BetterRecall.Agent/native/libvpl.dll`: oneVPL dispatcher (MIT, Intel), licence in `libvpl.LICENSE.txt`
- NuGet runtimes with their own terms, including Windows App SDK, .NET, OpenVINO, ONNX Runtime, SQLite3MultipleCiphers and SkiaSharp
