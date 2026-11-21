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
