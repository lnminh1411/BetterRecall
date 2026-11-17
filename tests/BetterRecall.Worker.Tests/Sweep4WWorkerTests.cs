// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Text.Json;
using BetterRecall.Contracts;
using BetterRecall.Worker.Engine;
using BetterRecall.Worker.Hosting;
using BetterRecall.Worker.Inference;
using BetterRecall.Worker.Inference.Onnx;
using BetterRecall.Worker.Models;
using BetterRecall.Worker.Protocol;

namespace BetterRecall.Worker.Tests;

/// <summary>Bug sweep 4, batch W: worker halves of A-M3 (replies name the embedder, replan keeps the model), A-M4
/// (hello without an embedder) and C5 (settings from the agent, not settings.json).</summary>
public class Sweep4WWorkerTests
{
    private static WorkerReply Send(RequestHandler h, WorkerRequest r, byte[]? payload = null)
        => h.Handle(JsonSerializer.SerializeToUtf8Bytes(r, ContractsJson.Default.WorkerRequest), payload ?? []);

    [Fact]
    public void Embed_and_bench_replies_name_the_embedder()
    {
        var home = TestEnv.NewDir("s4w-id");
        var s = TestEnv.Settings(home, x => x.Index.Dim = 256);
        var h = new RequestHandler(new FakeEngine(), () => s);
        var text = Send(h, new WorkerRequest(1, "embed_text", Text: "hello", Kind: "query")).Header;
        var image = Send(h, new WorkerRequest(2, "embed_image", Format: "bgra8", Width: 2, Height: 2, Stride: 8), new byte[16]).Header;
        var bench = Send(h, new WorkerRequest(3, "bench")).Header;
        foreach (var r in new[] { text, image, bench })
        {
            Assert.True(r.Ok, r.Error);
            Assert.Equal(FakeEngine.ModelId, r.Model);
            Assert.Equal(FakeEngine.Revision, r.Revision);
            Assert.Equal("CPU", r.Device);
        }
        Assert.Null(Send(h, new WorkerRequest(4, "shutdown")).Header.Model);
    }

    /// <summary>A-M4 worker half: with no embedder the hello still answers (the agent needs the worker for OCR and
    /// speech) but with an empty revision, which the agent refuses as an embedding space; embed replies name nothing.</summary>
    [Fact]
    public void Hello_without_an_embedder_has_an_empty_revision_and_names_nothing()
    {
        var home = TestEnv.NewDir("s4w-noemb");
        var s = TestEnv.Settings(home, x => { x.Privacy.NsfwFilter = false; x.Ocr.Enabled = false; });
        using var engine = new RealEngine(s, home, new OnnxBackend(cpuOnly: true), new OnnxBackend(cpuOnly: true));
        var h = new RequestHandler(engine, () => s);
        var hello = Send(h, new WorkerRequest(1, "hello")).Header.Hello!;
        Assert.Equal("", hello.Revision);
        Assert.Contains(ModelCatalog.EmbeddingGemma, hello.Missing);
        Assert.Null(engine.Embedder);
        var embed = Send(h, new WorkerRequest(2, "embed_text", Text: "x", Kind: "query")).Header;
        Assert.False(embed.Ok);
        Assert.StartsWith("Model not installed", embed.Error);
        Assert.Null(embed.Model);
    }

    /// <summary>C5 worker half: the agent's worker keys (stdin) win over settings.json; a key the input leaves out takes
    /// its default, not the file's value. Unreadable, empty or oversized input gives no settings at all (SF1: the worker
    /// then exits with 2), never the defaults.</summary>
    [Fact]
    public void Agent_settings_input_wins_over_settings_json_and_fails_closed()
    {
        var home = TestEnv.NewDir("s4w-args");
        var file = TestEnv.Settings(home, x => { x.Index.Dim = 768; x.Inference.Device = "cpu"; x.Ocr.Languages = "de"; });
        Assert.Equal(768, file.Dim);
        var s = WorkerSettings.TryFromJson("""{"index":{"dim":512},"ocr":{"languages":"en"}}""", home, out _)!;
        Assert.Equal(512, s.Dim);
        Assert.Equal("en", s.OcrLanguages);
        Assert.Equal("auto", s.Device);
        Assert.Equal(140, s.ImageTokens);
        Assert.Null(WorkerSettings.TryFromJson("not json", home, out var why));
        Assert.NotEmpty(why);
        Assert.Null(WorkerSettings.TryFromJson("[1,2]", home, out _));

        static string? Read(byte[] b, out string why) => WorkerSettings.ReadAgentInput(new MemoryStream(b), out why);
        Assert.Equal("{}", Read("{}"u8.ToArray(), out _));
        Assert.Null(Read([], out why));
        Assert.Equal("missing", why);
        Assert.Null(Read(new byte[WorkerSettings.MaxAgentInputBytes + 1], out why));
        Assert.StartsWith("larger than", why);
        Assert.Null(Read([0xC3, 0x28], out _));   // not UTF-8

        var name = PipeNames.NewWorker();
        Assert.True(WorkerArgs.Parse(["--pipe", name, "--home", home, "--settings-stdin"])!.SettingsFromStdin);
        Assert.False(WorkerArgs.Parse(["--pipe", name, "--home", home])!.SettingsFromStdin);
        Assert.Null(WorkerArgs.Parse(["--pipe", name, "--home", home, "--settings", "{}"]));   // never on the command line
        Assert.Null(WorkerArgs.Parse(["--pipe", name, "--home"]));
    }

    /// <summary>A-M3 worker half: the cached choice is SigLIP on the GPU; the GPU is lost at run time. The replan used
    /// to bench afresh and pick EmbeddingGemma on the CPU (another model, no new hello, one index). It now keeps SigLIP,
    /// on the CPU, and the next reply says so.</summary>
    [Fact]
    public void Replan_after_a_device_loss_keeps_the_model()
    {
        var home = TestEnv.NewDir("s4w-replan");
        var models = Path.Combine(home, "models");
        TestEnv.InstallEmbeddingGemma(models);
        TestEnv.InstallSiglip(models);
        var s = TestEnv.Settings(home, x => { x.Inference.Device = "gpu"; x.Privacy.NsfwFilter = false; x.Ocr.Enabled = false; });
        using (var first = new RealEngine(s, home, new Sweep2GTests.FlakyGpuBackend(), new OnnxBackend(cpuOnly: true)))
            Assert.Equal("synthetic/embeddinggemma-2", first.Hello(768).Model);
        // The cached choice names SigLIP on the GPU (the NPU 3720-class path, where EmbeddingGemma fails).
        var cachePath = DataPaths.DeviceCache(home);
        var cache = JsonSerializer.Deserialize(File.ReadAllBytes(cachePath), WorkerJson.Default.DeviceCacheFile)!;
        Assert.Equal("GPU.0", cache.Chosen!.DeviceId);
        cache.Chosen = cache.Chosen with { Model = ModelCatalog.Siglip };
        DeviceCache.Write(cachePath, cache);

        var gpu = new Sweep2GTests.FlakyGpuBackend();
        using var engine = new RealEngine(s, home, gpu, new OnnxBackend(cpuOnly: true));
        var h = new RequestHandler(engine, () => s);
        Assert.Equal("synthetic/siglip2-base", Send(h, new WorkerRequest(1, "hello")).Header.Hello!.Model);
        gpu.Broken = true;
        for (int i = 0; i < RealEngine.RunFailuresBeforeReplan; i++)
            Assert.False(Send(h, new WorkerRequest(2 + i, "embed_text", Text: "lost", Kind: "query")).Header.Ok);
        var after = Send(h, new WorkerRequest(9, "embed_text", Text: "after", Kind: "query")).Header;
        Assert.True(after.Ok, after.Error);
        Assert.Equal("synthetic/siglip2-base", after.Model);
        Assert.Equal("CPU", after.Device);
    }

    /// <summary>SF6 (review): a replan that has to switch model (the kept one now fails on every device left) persists
    /// the switch. device.json used to keep the old model, so the next worker switched back and the agent queued the
    /// whole history twice per worker cycle.</summary>
    [Fact]
    public void Replan_that_switches_model_is_persisted()
    {
        var home = TestEnv.NewDir("s4w-switch");
        var models = Path.Combine(home, "models");
        TestEnv.InstallEmbeddingGemma(models);
        TestEnv.InstallSiglip(models);
        var s = TestEnv.Settings(home, x => { x.Inference.Device = "gpu"; x.Privacy.NsfwFilter = false; x.Ocr.Enabled = false; });
        using (var first = new RealEngine(s, home, new EgBreakingBackend(), new OnnxBackend(cpuOnly: true)))
            Assert.Equal("synthetic/embeddinggemma-2", first.Hello(768).Model);

        var b = new EgBreakingBackend();
        using (var engine = new RealEngine(s, home, b, new OnnxBackend(cpuOnly: true)))
        {
            var h = new RequestHandler(engine, () => s);
            Assert.Equal("synthetic/embeddinggemma-2", Send(h, new WorkerRequest(1, "hello")).Header.Hello!.Model);
            b.GpuBroken = true;   // the GPU is lost, and EmbeddingGemma no longer loads anywhere
            b.EgBroken = true;
            for (int i = 0; i < RealEngine.RunFailuresBeforeReplan; i++)
                Assert.False(Send(h, new WorkerRequest(2 + i, "embed_text", Text: "lost", Kind: "query")).Header.Ok);
            var after = Send(h, new WorkerRequest(9, "embed_text", Text: "after", Kind: "query")).Header;
            Assert.True(after.Ok, after.Error);
            Assert.Equal("synthetic/siglip2-base", after.Model);
        }
        var cache = JsonSerializer.Deserialize(File.ReadAllBytes(DataPaths.DeviceCache(home)), WorkerJson.Default.DeviceCacheFile)!;
        Assert.Equal(ModelCatalog.Siglip, cache.Chosen!.Model);
        // The next worker process says hello with the model the agent now files under: no switch back.
        using var next = new RealEngine(s, home, new EgBreakingBackend(), new OnnxBackend(cpuOnly: true));
        Assert.Equal("synthetic/siglip2-base", next.Hello(768).Model);
    }

    /// <summary>Sweep2G's flaky GPU plus a switch that fails every EmbeddingGemma load (a broken runtime, out of memory).</summary>
    private sealed class EgBreakingBackend : IInferenceBackend
    {
        private readonly Sweep2GTests.FlakyGpuBackend _inner = new();
        public volatile bool EgBroken;
        public bool GpuBroken { set => _inner.Broken = value; }
        public string Name => _inner.Name;
        public string Version => _inner.Version;
        public string NpuDriverVersion => _inner.NpuDriverVersion;
        public IReadOnlyList<DeviceInfo> Devices => _inner.Devices;

        public IInferenceSession Load(string modelPath, DeviceInfo device, string? cacheDir, IReadOnlyDictionary<string, long[]>? staticShapes = null)
        {
            if (EgBroken && modelPath.Contains(ModelCatalog.EmbeddingGemma, StringComparison.OrdinalIgnoreCase))
                throw new DeviceFailedException("runtime broken");
            return _inner.Load(modelPath, device, cacheDir, staticShapes);
        }

        public void Dispose() => _inner.Dispose();
    }
}
