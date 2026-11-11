# Scripts/models

Model preparation for `BetterRecall.Worker`. Every script writes into the models folder
(`%LOCALAPPDATA%\BetterRecall\models\`, or `BETTERRECALL_MODELS`) a subfolder with the converted graphs and a
`manifest.json` (file, size, sha256, source URL, licence, revision, tensor names, shapes, preprocessing). The worker
reads only the manifest. Its SHA-256 is the "model hash" in `cache\device.json` and `cache\compiled\`.

Downloads go to the Hugging Face cache at `%LOCALAPPDATA%\BetterRecall\cache\hf` (`HF_HOME`). Model weights are never committed; the converted folders are published at `huggingface.co/lnminh1411/BetterRecall-models`.

## Downloads (each needs the user's go-ahead)

| What | Source (official publisher, pinned) | Licence | Size | Used for |
|---|---|---|---|---|
| EmbeddingGemma 2: `model.safetensors`, `tokenizer.model`, configs | `huggingface.co/google/embeddinggemma-2` @ `f9972ac2` | Apache-2.0 | 1.53 GB | Image and text vectors (text + vision towers; audio tower dropped) |
| SigLIP 2 base patch16-224: `model.safetensors`, `tokenizer.model`, configs | `huggingface.co/google/siglip2-base-patch16-224` @ `75de2d55` | Apache-2.0 | 1.54 GB | NPU fallback when the EmbeddingGemma 2 vision tower fails there |
| Whisper large-v3-turbo: `model.safetensors`, tokenizer + configs | `huggingface.co/openai/whisper-large-v3-turbo` @ `41f01f3f` | MIT | 1.62 GB | Speech to text (multilingual, auto language) |
| Silero VAD v6.2.3: `silero_vad.onnx` | `github.com/snakers4/silero-vad` tag `v6.2.3`, `src/silero_vad/data/` | MIT | 2.3 MB | Speech gate before Whisper (CPU) |
| NSFW classifier: `model.safetensors`, configs (not `optimizer.pt`, `pytorch_model.bin`, the YOLO `.pt`) | `huggingface.co/Falconsai/nsfw_image_detection` @ `96cb0d03` | Apache-2.0 | 345 MB | NSFW gate before embedding (ViT-base/16, labels `normal`/`nsfw`; INT8 on the CPU, FP32 on NPU/GPU) |
| NSFW detector: `320n.onnx` only | `huggingface.co/deepghs/nudenet_onnx` @ `44956a05` (ONNX of notAI-tech/NudeNet) | AGPL-3.0 (repo tagged Apache-2.0, upstream NudeNet is AGPL-3.0) | 12 MB | Second NSFW model (YOLOv8n body-part detector at 640 px), used as is |
| OpenVINO GenAI 2026.4.1 runtime archive (not a model) | `storage.openvinotoolkit.org/repositories/openvino_genai/packages/2026.4.1/windows/openvino_genai_windows_2026.4.1.0_x86_64.zip` | Apache-2.0 | 254 MB | `openvino_genai_c.dll` for Whisper; staged into `artifacts\native\openvino-genai\` |

Total about 5.3 GB downloaded. Converted output on disk is about 2.5 GB: EmbeddingGemma 2 about 1.3 GB
(text graph, three vision graphs, bf16 token table), SigLIP 2 about 0.75 GB, Whisper INT8 about 0.85 GB, NSFW INT8
about 90 MB plus its FP32 graph about 345 MB, NSFW detector 12 MB.

None of the repositories is gated. No login or licence click-through is needed.

## Python environments

Two venvs because `optimum-intel` 2.2 pins `transformers < 5.6`, and EmbeddingGemma 2 needs `transformers >= 5.18`.
`prepare_all.py` puts them under `%BETTERRECALL_DEV%` (default `%LOCALAPPDATA%\BetterRecall-dev`).

```
python -m venv venv
venv\Scripts\pip install torch torchvision --index-url https://download.pytorch.org/whl/cpu
venv\Scripts\pip install transformers==5.22.2 sentence-transformers==6.2.0 openvino==2026.4.1 openvino-tokenizers==2026.4.1.0 onnx onnxruntime onnxscript pillow huggingface_hub safetensors accelerate sentencepiece protobuf

python -m venv venv-whisper
venv-whisper\Scripts\pip install torch --index-url https://download.pytorch.org/whl/cpu
venv-whisper\Scripts\pip install "optimum-intel[openvino]==2.2.0" openvino==2026.4.1 openvino-tokenizers==2026.4.1.0 openvino-genai==2026.4.1.0 nncf
```

## Scripts

| Script | What it does | Run with |
|---|---|---|
| `common.py` | Pinned sources, paths, sha256, manifest writer, cosine check, the worker's benchmark frame | imported |
| `fetch_models.py [names]` | Downloads the pinned files (all by default) into `HF_HOME`; Silero VAD straight into `models\silero-vad\` | `venv` |
| `export_embeddinggemma2.py` | Text graph (dynamic length, inputs_embeds) + vision graphs at 70/140/280 tokens + bf16 token table; checks OpenVINO CPU and NPU against sentence-transformers by cosine (>= 0.999), NaN, first-compile time, latency. `--random-init --config-dir DIR --no-save` checks the architecture without weights | `venv` |
| `export_siglip2.py` | SigLIP 2 image and text towers, static shapes; cosine on CPU and NPU | `venv` |
| `export_onnx.py [--out DIR] [--affinity MASK]` | float32 ONNX twins of the embedder graphs for ONNX Runtime (DirectML cards): EmbeddingGemma 2 `text.onnx` (dynamic length) and `vision.onnx` (dynamic patches and budget, one graph for 70/140/280), SigLIP 2 `text.onnx` and `vision.onnx`; adds the `onnx` map (IR file -> ONNX file) to the manifest; ONNX Runtime CPU vs PyTorch cosine >= 0.9999. `--out` writes a complete copy (hard links) elsewhere; `--affinity` pins to those CPUs at below-normal priority | `venv` |
| `export_whisper.py` | `optimum-cli export openvino --weight-format int8` for GenAI WhisperPipeline; `--tts out.wav` makes an offline SAPI test clip and reports compile time and real-time factor on NPU and CPU | `venv-whisper` |
| `export_nsfw.py` | ViT to ONNX: dynamic INT8 (per-channel) for the CPU and the FP32 graph for NPU/GPU, FP32 vs INT8 agreement report | `venv` |
| `export_nsfw_detector.py [--from FILE]` | Verifies the pinned NudeNet `320n.onnx` (size, SHA-256), copies it to `models\nsfw-detector\model.onnx`, checks the output shape, writes the manifest | `venv` |
| `stage_genai_runtime.py` | Downloads and verifies the GenAI archive, copies `openvino_genai*.dll` and tokenizer DLLs for the worker build | `venv` |

Order: `fetch_models.py`, then the five exports and `export_onnx.py`, then `stage_genai_runtime.py`, then rebuild the worker.

## What is verified, what is not

- Verified without weights (2026-11-10): the EmbeddingGemma 2 text and vision graphs convert to OpenVINO IR and compile
  on the Arrow Lake NPU from a random-initialised model built from the official `config.json`; the official
  `tokenizer.model` gives identical ids in Python SentencePiece and Microsoft.ML.Tokenizers (worker test
  `Real_gemma_tokenizer_matches_sentencepiece_when_available`).
- Verified with real weights (2026-11-10, Core Ultra 7 265K, NPU driver 32.0.100.5540): EmbeddingGemma 2 on OpenVINO
  CPU and NPU against sentence-transformers / the official Gemma 4 processor, cosine >= 0.9999 for text and for images
  at 70/140/280 tokens in 16:9, 16:10, 4:3, 21:9 and portrait, no NaN. The vision graphs take the processor's own input
  (aspect-preserving resize, patches, positions, plus the 3x3 pooling matrix) so any aspect ratio matches; an earlier
  fixed 16:9 graph stretched other shapes (4:3 at 280 tokens: cosine 0.987). The pooling matrix is an input because the
  NPU rejects the 280-token graph at inference when it builds that matrix from runtime positions. Whisper INT8 runs on
  NPU and CPU through GenAI (the optimum export warnings "Failed to build model wrapper" and "fun_asr" are harmless).
- Graph inputs carry exactly one tensor name each (`name_inputs`): the worker reads names with `ov_port_get_any_name`.
- Still open: SigLIP 2 lower-casing, NSFW INT8 accuracy on real content (only label agreement with the float model).
