# Licensing

BetterRecall is published by its contributors and licensed under the GNU Affero General Public License, version 3 only (AGPL-3.0-only). The full text is in `LICENSE` and `LICENSES/AGPL-3.0.txt`. Every source file starts with an `SPDX-License-Identifier: AGPL-3.0-only` line.

This covers all of the code: the agent, worker, core, CLI, MCP server, installer, the user interface, the shared contracts, the tests and the scripts. If you change BetterRecall and let people use it over a network, you must offer them your source.

Until version 0.3, the WinUI app (`src/BetterRecall.UI`), the shared contracts (`src/BetterRecall.Contracts`, `rust/crates/br-contracts`) and the UI tests were MIT. From 0.4.0-dev they are AGPL-3.0-only like the rest. Copies of those parts taken from earlier commits keep the MIT terms they were published under.

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
