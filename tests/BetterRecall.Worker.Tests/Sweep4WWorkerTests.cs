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

    /// <summary>C5 worker half: --settings (the agent's worker keys) wins over settings.json; a key the argument leaves
    /// out takes its default, not the file's value.</summary>
    [Fact]
    public void Settings_argument_wins_over_settings_json()
    {
        var home = TestEnv.NewDir("s4w-args");
        var file = TestEnv.Settings(home, x => { x.Index.Dim = 768; x.Inference.Device = "cpu"; x.Ocr.Languages = "de"; });
        Assert.Equal(768, file.Dim);
        var s = WorkerSettings.FromJson("""{"index":{"dim":512},"ocr":{"languages":"en"}}""", home);
        Assert.Equal(512, s.Dim);
        Assert.Equal("en", s.OcrLanguages);
        Assert.Equal("auto", s.Device);
        Assert.Equal(140, s.ImageTokens);
        Assert.Equal(768, WorkerSettings.FromJson("not json", home).Dim);   // unreadable: defaults, never the file

        var name = PipeNames.NewWorker();
        var args = WorkerArgs.Parse(["--pipe", name, "--home", home, "--settings", """{"index":{"dim":256}}"""]);
        Assert.NotNull(args);
        Assert.Equal("""{"index":{"dim":256}}""", args!.Settings);
        Assert.Null(WorkerArgs.Parse(["--pipe", name, "--home", home])!.Settings);
        Assert.Null(WorkerArgs.Parse(["--pipe", name, "--home", home, "--settings"]));
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
}
