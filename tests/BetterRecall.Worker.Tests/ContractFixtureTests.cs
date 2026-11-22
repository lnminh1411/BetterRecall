// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Text;
using System.Text.Json;
using BetterRecall.Contracts;

namespace BetterRecall.Worker.Tests;

public class ContractFixtureTests
{
    private static string RepoRoot()
    {
        var d = new DirectoryInfo(AppContext.BaseDirectory);
        while (d is not null && !File.Exists(Path.Combine(d.FullName, "BetterRecall.slnx"))) d = d.Parent;
        return d?.FullName ?? throw new InvalidOperationException("repo root not found");
    }

    private static string Serialize<T>(T value)
        => JsonSerializer.Serialize(value, typeof(T), ContractsJson.Default);

    private static string ExtractMcpToolList()
    {
        var mcpSource = File.ReadAllText(Path.Combine(RepoRoot(), "src", "BetterRecall.Mcp", "McpServer.cs"), Encoding.UTF8);
        const string startMarker = "private const string ToolsJson = \"\"\"";
        int start = mcpSource.IndexOf(startMarker, StringComparison.Ordinal);
        Assert.True(start >= 0, "ToolsJson start marker not found in McpServer.cs");
        start += startMarker.Length;
        int end = mcpSource.IndexOf("\"\"\";", start, StringComparison.Ordinal);
        Assert.True(end > start, "ToolsJson end marker not found in McpServer.cs");
        using var toolsDoc = JsonDocument.Parse(mcpSource[start..end]);
        var buf = new System.Buffers.ArrayBufferWriter<byte>();
        using (var w = new Utf8JsonWriter(buf))
        {
            w.WriteStartObject();
            w.WritePropertyName("tools");
            toolsDoc.RootElement.WriteTo(w);
            w.WriteEndObject();
        }
        return Encoding.UTF8.GetString(buf.WrittenSpan);
    }

    [Fact]
    public void Worker_contract_fixtures_match_contracts_json()
    {
        var dir = Path.Combine(RepoRoot(), "tests", "fixtures", "contracts");
        var fixtures = new (string Name, string Json)[]
        {
            ("WorkerRequest", Serialize(new WorkerRequest(
                Id: 42,
                Op: "embed_image",
                Format: "bgra8",
                Width: 1280,
                Height: 720,
                Stride: 5120,
                Text: "quarterly sales",
                Kind: "query",
                SampleRate: 16000,
                Channels: 1,
                Source: "mic",
                Language: "en",
                Nsfw: 0.8,
                NsfwGrid: 2,
                NsfwDetector: 0.4,
                Entities: true))),
            ("WorkerResponse", Serialize(new WorkerResponse(
                Id: 42,
                Ok: true,
                Dim: 768,
                Ms: 12.5,
                Model: "google/embeddinggemma-2",
                Revision: "rev1",
                Device: "NPU"))),
            ("WorkerHello", Serialize(new WorkerHello(
                Protocol: 1,
                Device: "NPU",
                Ep: "OpenVINO",
                Model: "google/embeddinggemma-2",
                Revision: "rev1",
                Dim: 768,
                Asr: false,
                Vad: true,
                Missing: ["whisper-large-v3-turbo"],
                NsfwDevice: "NPU",
                OcrDevice: "onnx iGPU"))),
            ("TranscriptSegment", Serialize(new TranscriptSegment(
                StartMs: 120,
                EndMs: 1840,
                Text: "hello world"))),
            ("TranscribeResult", Serialize(new TranscribeResult(
                Speech: true,
                Text: "hello world",
                Language: "en",
                Segments: [new TranscriptSegment(120, 1840, "hello world")],
                Ms: 45))),
            ("BenchDevice", Serialize(new BenchDevice(
                Device: "GPU",
                Ep: "OpenVINO",
                Ok: true,
                ImageMs: 0,
                TextMs: 3.5,
                Cosine: 1.0,
                Error: null,
                Model: "embeddinggemma-2",
                TextDevice: "CPU",
                CompileMs: 420,
                Adapter: "Intel(R) Arc(TM) Graphics",
                AdapterAuto: true))),
            ("TokenBench", Serialize(new TokenBench(
                Device: "NPU",
                ImageTokens: 140,
                ImageMs: 12.3))),
            ("BenchResult", Serialize(new BenchResult(
                Chosen: "GPU",
                Devices:
                [
                    new BenchDevice("GPU", "OpenVINO", true, 0, 3.5, 1.0, null, "embeddinggemma-2", "CPU", 420, "Intel(R) Arc(TM) Graphics", true),
                    new BenchDevice("NPU", "OpenVINO", false, 0, 0, 0, "NPU compile failed.", "embeddinggemma-2"),
                ],
                Tokens: [new TokenBench("GPU", 140, 12.3)],
                ChosenAdapter: "Intel(R) Arc(TM) Graphics"))),
            ("OcrEntity", Serialize(new OcrEntity(
                Start: 0,
                Length: 8,
                Label: "person",
                Score: 0.95f))),
            ("OcrLine", Serialize(new OcrLine(
                Text: "Jane Doe",
                X: 8,
                Y: 12,
                W: 80,
                H: 16,
                Confidence: 1f,
                Cuts: [8, 18, 28, 38, 48, 58, 68, 78, 88],
                Entities: [new OcrEntity(0, 8, "person", 0.95f)]))),
            ("OcrResult", Serialize(new OcrResult(
                Lines:
                [
                    new OcrLine("Jane Doe", 8, 12, 80, 16, 1f, [8, 18, 28, 38, 48, 58, 68, 78, 88], [new OcrEntity(0, 8, "person", 0.95f)]),
                    new OcrLine("ok", 8, 36, 20, 16, 0.98f, [8, 18, 28]),
                ],
                Engine: "onnx",
                Ms: 18.25,
                EntitiesMissing: false))),
            ("Settings", Serialize(new Settings())),
            ("AgentStatus", Serialize(new AgentStatus(
                Capturing: true,
                Paused: false,
                PausedUntil: null,
                PauseReason: null,
                Locked: false,
                UnlockedUntil: new DateTimeOffset(2026, 11, 21, 10, 15, 0, TimeSpan.Zero),
                Audio: new AudioState(false, true),
                Queue: new QueueState(3),
                Device: "NPU",
                Store: new StoreStats(
                    Rows: 120,
                    Bytes: 65536,
                    ThumbBytes: 4096,
                    Oldest: new DateTimeOffset(2026, 11, 1, 8, 30, 0, TimeSpan.Zero),
                    Newest: new DateTimeOffset(2026, 11, 21, 9, 59, 30, 500, TimeSpan.Zero),
                    PendingEmbeddings: 2,
                    Vectors: 118),
                RetentionDays: 30,
                HotkeysFailed: null,
                Warnings: ["ocr-failing"],
                UserPaused: false,
                UnlockNeeds: "none"))),
            ("SearchResponse", Serialize(new SearchResponse(
                Count: 2,
                Truncated: false,
                TookMs: 14,
                Results:
                [
                    new SearchHit(
                        Id: "s_01ABC",
                        Ts: new DateTimeOffset(2026, 11, 21, 9, 30, 0, TimeSpan.Zero),
                        Kind: CaptureKind.Screen,
                        App: "chrome.exe",
                        Title: "Quarterly report",
                        Score: 0.875f,
                        Snippet: "...quarterly revenue...",
                        HasThumb: true,
                        DurationS: null,
                        Cosine: 0.9f,
                        TextMatch: true,
                        Weak: false,
                        Site: "docs.example.com"),
                    new SearchHit(
                        Id: "r_01DEF",
                        Ts: new DateTimeOffset(2026, 11, 21, 9, 45, 12, 250, TimeSpan.Zero),
                        Kind: CaptureKind.Replay,
                        App: "code.exe",
                        Title: "main.rs",
                        Score: 0.5f,
                        Snippet: "fn main()",
                        HasThumb: true,
                        DurationS: null,
                        Cosine: 0.5f,
                        TextMatch: false,
                        Weak: true,
                        Site: "",
                        ReplayId: "20261121T094500Z",
                        ReplayOffsetMs: 12250),
                ],
                Degraded: null))),
            ("ErrorReply", Envelope.Error("Store is locked.")),
            ("McpToolList", ExtractMcpToolList()),
            ("ModelsList", Serialize(new ModelsList(
                ModelsDir: @"C:\BetterRecall\models",
                Repo: "lnminh1411/BetterRecall-models",
                Revision: "893eb2c63a11443e9f5061745c69ecbd5e1bd16a",
                Components:
                [
                    new ModelComponent("embeddinggemma-2", "EmbeddingGemma 2", "Search", ["full", "minimal"], 2782236945, "complete"),
                    new ModelComponent("silero-vad", "Silero VAD", "Speech detection for transcription", ["full"], 2329089, "missing"),
                ],
                FullBytes: 6555959572,
                MinimalBytes: 3476218217,
                FreeBytes: 53687091200,
                Downloading: false))),
            ("ModelsProgress", Serialize(new ModelsProgress(
                Phase: "downloading",
                DoneBytes: 524288000,
                TotalBytes: 2782236945,
                File: "embeddinggemma-2/vision.onnx"))),
            ("UpdateStatus", Serialize(new UpdateStatus(
                CurrentVersion: "0.3.0-dev",
                LatestVersion: "0.3.0",
                Available: true,
                CanInstall: true,
                Reason: null,
                NotesUrl: "https://github.com/lnminh1411/BetterRecall/releases/tag/v0.3.0",
                PublishedAt: new DateTimeOffset(2026, 11, 22, 10, 0, 0, TimeSpan.Zero),
                InstallerBytes: 125829120,
                State: "idle",
                DownloadedVersion: null,
                LastCheck: new DateTimeOffset(2026, 11, 22, 10, 5, 0, TimeSpan.Zero),
                Auto: false,
                Error: null))),
            ("UpdateProgress", Serialize(new UpdateProgress(
                Phase: "downloading",
                DoneBytes: 52428800,
                TotalBytes: 125829120,
                Error: null))),
        };

        bool update = Environment.GetEnvironmentVariable("BR_UPDATE_FIXTURES") == "1";
        if (update) Directory.CreateDirectory(dir);
        foreach (var (name, json) in fixtures)
        {
            var path = Path.Combine(dir, $"{name}.json");
            if (update) File.WriteAllText(path, json, new UTF8Encoding(false));
            Assert.True(File.Exists(path), $"Missing contract fixture: {path}");
            var onDisk = File.ReadAllText(path, Encoding.UTF8);
            Assert.Equal(json, onDisk);
        }
    }
}
