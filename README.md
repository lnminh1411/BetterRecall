**[AI Content Warning: AI Agents was used to assist with coding]**

<p align="center"><img src="docs/images/banner.png" alt="BetterRecall: search everything you have seen and heard" /></p>

BetterRecall is a local alternative to Windows Recall. It keeps an encrypted history of your screen, and of speech it hears, and lets you search it in your own words: "the invoice from Acme", "that flight to Tokyo", "what was said in standup". Images and text are embedded on your NPU or GPU with EmbeddingGemma 2. The history stays on your PC, encrypted with a key held by the TPM behind Windows Hello. BetterRecall makes no network connections while cloud describe is off, which is the default. Turning cloud describe on ends that guarantee: a confirmation says so, and from then on BetterRecall can send a single image you have previewed to the AI provider you configured, only when you click Send. It also enables Ask, a chat that searches your history for you and answers with sources; each chat shows exactly which captures it would send, with sensitive details redacted, and sends them only after you allow it. Turn it off to restore the guarantee.

It also keeps an instant replay buffer, so you can save the last minutes of your screen as a video. It skips private browser windows, apps you exclude and explicit images.

<p align="center">
  <img src="docs/images/search-dark.png" width="49%" alt="Search window" />
  <img src="docs/images/home-dark.png" width="49%" alt="Home page, dark" />
</p>
<p align="center">
  <img src="docs/images/timeline-dark.png" width="49%" alt="Timeline" />
  <img src="docs/images/home-light.png" width="49%" alt="Home page, light" />
</p>

Screenshots show the real app running on demo data (`--mock`).

**Status:** early development, Windows 11 x64. There is no installer yet; build from source and download the models from [Hugging Face](https://huggingface.co/lnminh1411/BetterRecall-models). **Docs:** [`docs/`](docs/documentations_summary.md). **Licences:** the WinUI app and contracts are MIT, the rest is AGPL-3.0 ([`LICENSING.md`](LICENSING.md)); models keep their own licences.

## System requirements

Estimates for 0.3.0-dev, from the code's defaults and the dev PC's measurements. The first run's benchmark adjusts settings to the machine; the reasons below say what it changes.

**Minimum**

| Part | Requirement | Why |
|---|---|---|
| OS | Windows 11 (build 22000 or later), x64 | The exes target build 20348 or later, which no Windows 10 release reached; system audio uses process loopback, which needs it. Windows on Arm is untested (x64 build only) |
| Runtime | Nothing to install | Release builds ship the Microsoft Visual C++ 2015-2022 runtime DLLs next to the agent and worker (app-local), which the worker's OpenVINO and ONNX Runtime need. If they are missing, the agent says so instead of starting the worker |
| CPU | 4 cores, x64 | With no NPU or usable GPU, embeddings, screen text (OCR) and speech run on the CPU: it works, but indexing lags behind and costs more power (OCR alone is about 1 s of CPU per screen) |
| RAM | 8 GB | Below 8 GB the benchmark cuts the replay to 60 s and, with long retention, the vectors to 512 dimensions. The replay ring stays in RAM (about 90 MB per monitor at the default 120 s and 6 Mbit/s) |
| GPU | DirectX 11 GPU with a WDDM driver | Screen capture uses Desktop Duplication. Replay also needs a hardware video encoder (H.264, HEVC or AV1); with none the benchmark turns replay off |
| Disk | SSD with about 35 GB free | The app takes about 0.6 GB and the models 4 to 6 GB. The history may grow to `storage.maxGb` (20 GB by default), and capture pauses when free space drops under `storage.minFreeGb` (25 GB by default); lower both on a small disk |
| Security | TPM 2.0 recommended | Without a TPM the data key is protected with DPAPI or a passphrase. Without Windows Hello, an unlock password can guard reads |

**Recommended**

| Part | Recommendation | Why |
|---|---|---|
| OS | Windows 11 24H2 or later | The tested path |
| CPU | Intel Core Ultra (Meteor Lake or later) | Its NPU runs embeddings, OCR and the NSFW check through OpenVINO at almost no CPU cost (OCR about 0.1 s of CPU per screen). Keyframe encoding measured 0.45% CPU on average. AMD and NVIDIA systems use the GPU through DirectML instead; other NPUs are not used yet |
| RAM | 16 GB | Full 768-dimension vectors, the 120 s replay and room for the worker's models |
| GPU | GPU with an AV1 encoder (Intel Arc or Xe, NVIDIA RTX 40, AMD RX 7000) | Smallest replay clips, with encoding off the CPU |
| Disk | NVMe SSD with 64 GB or more free | Room for the default 20 GB history cap, the 25 GB free-space reserve, models and replay clips |
| Security | TPM 2.0 plus Windows Hello (face or fingerprint) | Silent key unlock and a quick read gate |

## Build

Requirements on Windows x64:

- .NET 10 SDK (`10.0.403` or later) and Visual Studio Build Tools with the C++ workload (for Native AOT linking and the MSVC linker).
- Rust toolchain via [`rustup`](https://rustup.rs): `stable` toolchain with the `x86_64-pc-windows-msvc` target. The inference worker (`BetterRecall.Worker.exe`) is written in Rust (`rust\crates\br-worker`) and built automatically through `src\BetterRecall.Worker\BetterRecall.Worker.csproj`.

Build the solution with `dotnet build BetterRecall.slnx`, or build a full layout into `artifacts\dev\` with `python Scripts/dev_build.py` (add `--test` to run `cargo test` and the `.NET` test suites first). See `Scripts/models/prepare_all.py` or download the exported models from [Hugging Face](https://huggingface.co/lnminh1411/BetterRecall-models).

