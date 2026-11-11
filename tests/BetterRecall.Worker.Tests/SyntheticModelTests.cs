// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using BetterRecall.Contracts;
using BetterRecall.Worker.Engine;
using BetterRecall.Worker.Inference;
using BetterRecall.Worker.Inference.Onnx;
using BetterRecall.Worker.Inference.OpenVino;
using BetterRecall.Worker.Models;
using BetterRecall.Worker.Protocol;
using BetterRecall.Worker.Vectors;
using Xunit.Abstractions;

namespace BetterRecall.Worker.Tests;

/// <summary>Real runtime paths with tiny synthetic ONNX graphs: ONNX Runtime CPU, OpenVINO CPU, and OpenVINO NPU when
/// this machine has one.</summary>
public class SyntheticModelTests(ITestOutputHelper output)
{
    private static (string Home, string Models) Home(string tag)
    {
        var home = TestEnv.NewDir(tag);
        var models = Path.Combine(home, "models");
        return (home, models);
    }

    [Fact]
    public void Ort_and_openvino_cpu_agree_on_the_text_graph()
    {
        var path = Path.Combine(TestEnv.NewDir("agree"), "text_16.onnx");
        File.Copy(TestEnv.Data("text_16.onnxt"), path);
        var ids = new long[16]; var mask = new long[16];
        for (int i = 0; i < 6; i++) { ids[i] = 10 + i; mask[i] = 1; }
        using var ort = new OnnxBackend(cpuOnly: true);
        using var s1 = ort.Load(path, ort.Devices[^1], null);
        var a = s1.Run([HostTensor.L("input_ids", ids, 1, 16), HostTensor.L("attention_mask", mask, 1, 16)])["embedding"].Floats!;
        using var ov = OpenVinoBackend.TryCreate();
        Assert.NotNull(ov);
        using var s2 = ov!.Load(path, ov.Devices.First(d => d.Kind == DeviceKind.Cpu), null);
        Assert.Equal(new long[] { 1, 16 }, s2.Inputs[0].Shape);
        var b = s2.Run([HostTensor.L("input_ids", ids, 1, 16), HostTensor.L("attention_mask", mask, 1, 16)])["embedding"].Floats!;
        Assert.Equal(768, a.Length);
        Assert.True(VectorMath.Cosine(a, b) > 0.99999);
        output.WriteLine($"OpenVINO {ov.Version}, ONNX Runtime {ort.Version}, devices {string.Join(", ", ov.Devices.Select(d => $"{d.Id}={d.Name}"))}");
    }

    [Fact]
    public async Task Real_engine_on_cpu_end_to_end_over_the_pipe()
    {
        var (home, models) = Home("realcpu");
        TestEnv.InstallEmbeddingGemma(models);
        TestEnv.InstallNsfw(models);
        TestEnv.InstallVad(models);
        var s = TestEnv.Settings(home, x => { x.Inference.Device = "cpu"; x.Index.Dim = 512; });
        using var engine = new RealEngine(s, home, new OnnxBackend(cpuOnly: true), new OnnxBackend(cpuOnly: true));
        await using var h = await PipeHarness.StartAsync(engine, s);

        var hello = (await h.CallAsync(new WorkerRequest(0, "hello"))).Header.Hello!;
        Assert.Equal("CPU", hello.Device);
        Assert.Equal("synthetic/embeddinggemma-2", hello.Model);
        Assert.Equal(512, hello.Dim);
        Assert.True(hello.Vad);
        Assert.False(hello.Asr);
        Assert.Equal(new[] { ModelCatalog.Siglip, ModelCatalog.Whisper, ModelCatalog.NsfwDetector }, hello.Missing);
        Assert.True(File.Exists(BetterRecall.Contracts.DataPaths.DeviceCache(home)));

        var (q, qp) = await h.CallAsync(new WorkerRequest(0, "embed_text", Text: "quarterly sales", Kind: "query"));
        Assert.True(q.Ok, q.Error);
        Assert.Equal(512 * 4, qp.Length);
        var (d, dp) = await h.CallAsync(new WorkerRequest(0, "embed_text", Text: "quarterly sales", Kind: "document"));
        Assert.NotEqual(qp, dp);
        // Long text goes to the 32 bucket and is truncated there.
        Assert.True((await h.CallAsync(new WorkerRequest(0, "embed_text", Text: string.Join(' ', Enumerable.Repeat("budget", 200)), Kind: "document"))).Header.Ok);

        var blue = TestEnv.Bgra(200, 120, 0, 0, 255);
        var (img, ip) = await h.CallAsync(new WorkerRequest(0, "embed_image", Format: "bgra8", Width: 200, Height: 120, Stride: 800, Nsfw: 0.8), blue);
        Assert.True(img.Ok, img.Error);
        Assert.Null(img.Dropped);
        Assert.Equal(512 * 4, ip.Length);

        var red = TestEnv.Bgra(200, 120, 255, 0, 0);
        var (drop, dpay) = await h.CallAsync(new WorkerRequest(0, "embed_image", Format: "bgra8", Width: 200, Height: 120, Stride: 800, Nsfw: 0.8), red);
        Assert.True(drop.Ok);
        Assert.Equal("nsfw", drop.Dropped);
        Assert.Empty(dpay);
        // No threshold: the classifier is skipped.
        Assert.Null((await h.CallAsync(new WorkerRequest(0, "embed_image", Format: "bgra8", Width: 200, Height: 120, Stride: 800), red)).Header.Dropped);

        var tr = new WorkerRequest(0, "transcribe", Format: "f32", SampleRate: 16000, Channels: 1, Source: "mic", Language: "auto");
        var (silence, _) = await h.CallAsync(tr, TestEnv.Pcm(0f));
        Assert.True(silence.Ok);
        Assert.False(silence.Transcript!.Speech);
        var (speech, _) = await h.CallAsync(tr, TestEnv.Pcm(0.2f));
        Assert.Equal("Model not installed: whisper-large-v3-turbo.", speech.Error);

        var bench = (await h.CallAsync(new WorkerRequest(0, "bench"))).Header.Bench!;
        Assert.Equal("CPU", bench.Chosen);
        Assert.All(bench.Devices, b => Assert.True(b.Ok));
    }

    [Fact]
    public async Task Nsfw_detector_drops_what_the_classifier_passes()
    {
        var (home, models) = Home("nsfwdet");
        TestEnv.InstallEmbeddingGemma(models);
        TestEnv.InstallNsfw(models);
        TestEnv.InstallNsfwDetector(models);
        var s = TestEnv.Settings(home, x => { x.Inference.Device = "cpu"; x.Privacy.NsfwDevice = "auto"; });
        using var engine = new RealEngine(s, home, new OnnxBackend(cpuOnly: true), new OnnxBackend(cpuOnly: true));
        await using var h = await PipeHarness.StartAsync(engine, s);

        var hello = (await h.CallAsync(new WorkerRequest(0, "hello"))).Header.Hello!;
        Assert.Equal("CPU", hello.NsfwDevice);
        Assert.DoesNotContain(ModelCatalog.NsfwDetector, hello.Missing);

        var green = TestEnv.Bgra(200, 120, 0, 255, 0);
        var blue = TestEnv.Bgra(200, 120, 0, 0, 255);
        var red = TestEnv.Bgra(200, 120, 255, 0, 0);
        WorkerRequest Req(double? nsfw, double? det) => new(0, "embed_image", Format: "bgra8", Width: 200, Height: 120, Stride: 800, Nsfw: nsfw, NsfwDetector: det);

        var (g, gp) = await h.CallAsync(Req(0.8, 0.5), green);       // classifier passes green, detector scores 1
        Assert.True(g.Ok, g.Error);
        Assert.Equal("nsfw", g.Dropped);
        Assert.Empty(gp);
        var (g2, g2p) = await h.CallAsync(Req(0.8, null), green);    // detector off: embedded
        Assert.Null(g2.Dropped);
        Assert.Equal(768 * 4, g2p.Length);
        var (b, bp) = await h.CallAsync(Req(0.8, 0.5), blue);        // both pass
        Assert.Null(b.Dropped);
        Assert.Equal(768 * 4, bp.Length);
        Assert.Equal("nsfw", (await h.CallAsync(Req(0.8, 0.5), red)).Header.Dropped);   // classifier blocks
        Assert.Equal("nsfw", (await h.CallAsync(Req(null, 0.5), green)).Header.Dropped); // detector alone
    }

    [Fact]
    public async Task Nsfw_detector_missing_fails_closed_only_when_requested()
    {
        var (home, models) = Home("nsfwnodet");
        TestEnv.InstallEmbeddingGemma(models);
        TestEnv.InstallNsfw(models);
        var s = TestEnv.Settings(home, x => x.Inference.Device = "cpu");
        using var engine = new RealEngine(s, home, new OnnxBackend(cpuOnly: true), new OnnxBackend(cpuOnly: true));
        await using var h = await PipeHarness.StartAsync(engine, s);
        var hello = (await h.CallAsync(new WorkerRequest(0, "hello"))).Header.Hello!;
        Assert.Equal("CPU", hello.NsfwDevice);   // classifier preloaded without the missing detector
        var blue = TestEnv.Bgra(8, 8, 0, 0, 255);
        Assert.Equal("Model not installed: nsfw-detector.",
            (await h.CallAsync(new WorkerRequest(0, "embed_image", Format: "bgra8", Width: 8, Height: 8, Stride: 32, Nsfw: 0.8, NsfwDetector: 0.5), blue)).Header.Error);
        Assert.True((await h.CallAsync(new WorkerRequest(0, "embed_image", Format: "bgra8", Width: 8, Height: 8, Stride: 32, Nsfw: 0.8), blue)).Header.Ok);
    }

    [Fact]
    public async Task Missing_models_and_broken_models_give_the_wire_errors()
    {
        var (home, models) = Home("missing");
        var s = TestEnv.Settings(home, x => x.Inference.Device = "cpu");
        using (var engine = new RealEngine(s, home, new OnnxBackend(cpuOnly: true), new OnnxBackend(cpuOnly: true)))
        {
            await using var h = await PipeHarness.StartAsync(engine, s);
            var hello = (await h.CallAsync(new WorkerRequest(0, "hello"))).Header.Hello!;
            Assert.Equal(6, hello.Missing.Length);
            Assert.Equal("Model not installed: embeddinggemma-2.", (await h.CallAsync(new WorkerRequest(0, "embed_text", Text: "x", Kind: "query"))).Header.Error);
            Assert.Equal("Model not installed: nsfw-classifier.",
                (await h.CallAsync(new WorkerRequest(0, "embed_image", Format: "bgra8", Width: 2, Height: 2, Stride: 8, Nsfw: 0.8), TestEnv.Bgra(2, 2, 1, 2, 3))).Header.Error);
            Assert.Equal("Model not installed: silero-vad.",
                (await h.CallAsync(new WorkerRequest(0, "transcribe", Format: "f32", SampleRate: 16000, Channels: 1, Source: "mic"), TestEnv.Pcm(0.2f))).Header.Error);
            Assert.Equal("Bad request.",
                (await h.CallAsync(new WorkerRequest(0, "embed_image", Format: "png"), [1, 2, 3, 4])).Header.Error);
        }

        TestEnv.InstallEmbeddingGemma(models, corruptVision: "this is not an onnx file");
        using (var engine = new RealEngine(s, home, new OnnxBackend(cpuOnly: true), new OnnxBackend(cpuOnly: true)))
        {
            await using var h = await PipeHarness.StartAsync(engine, s);
            var err = (await h.CallAsync(new WorkerRequest(0, "embed_text", Text: "x", Kind: "query"))).Header.Error!;
            Assert.StartsWith("Device failed: CPU ", err);
            Assert.EndsWith(".", err);
            output.WriteLine(err);
        }
    }

    [Fact]
    public void Siglip_fallback_when_primary_fails_everywhere()
    {
        var (home, models) = Home("siglip");
        TestEnv.InstallEmbeddingGemma(models, corruptVision: "broken");
        TestEnv.InstallSiglip(models);
        var s = TestEnv.Settings(home, x => x.Inference.Device = "cpu");
        using var engine = new RealEngine(s, home, new OnnxBackend(cpuOnly: true), new OnnxBackend(cpuOnly: true));
        var hello = engine.Hello(768);
        Assert.Equal("synthetic/siglip2-base", hello.Model);
        var v = engine.EmbedText("Some Query", TextKind.Query);
        Assert.Equal(768, v.Length);
        Assert.NotNull(engine.EmbedImage(new ImageInput("bgra8", 64, 64, 256, TestEnv.Bgra(64, 64, 9, 99, 199)), null).Vector);
    }

    [Fact]
    public void Device_cache_is_reused_and_invalidated_by_model_change()
    {
        var (home, models) = Home("reuse");
        TestEnv.InstallEmbeddingGemma(models);
        var s = TestEnv.Settings(home, x => x.Inference.Device = "cpu");
        using (var e1 = new RealEngine(s, home, new OnnxBackend(cpuOnly: true), new OnnxBackend(cpuOnly: true))) e1.Hello(768);
        var path = BetterRecall.Contracts.DataPaths.DeviceCache(home);
        var t1 = File.GetLastWriteTimeUtc(path);
        Thread.Sleep(50);
        using (var e2 = new RealEngine(s, home, new OnnxBackend(cpuOnly: true), new OnnxBackend(cpuOnly: true))) e2.Hello(768);
        Assert.Equal(t1, File.GetLastWriteTimeUtc(path));
        // Model hash change (manifest rewritten with another revision) forces a new benchmark.
        var mpath = Path.Combine(ModelCatalog.Dir(models, ModelCatalog.EmbeddingGemma), "manifest.json");
        File.WriteAllText(mpath, File.ReadAllText(mpath).Replace("\"syn1\"", "\"syn2\""));
        using (var e3 = new RealEngine(s, home, new OnnxBackend(cpuOnly: true), new OnnxBackend(cpuOnly: true))) Assert.Equal("syn2", e3.Hello(768).Revision);
        Assert.NotEqual(t1, File.GetLastWriteTimeUtc(path));
    }

    /// <summary>Table layout (bf16 token table + one dynamic text graph reshaped per length) on ONNX Runtime CPU,
    /// OpenVINO CPU and OpenVINO NPU: same vectors everywhere.</summary>
    [Fact]
    public void Table_layout_agrees_across_runtimes_and_npu()
    {
        var (home, models) = Home("table");
        TestEnv.InstallEmbeddingGemmaTableLayout(models);
        var m = ModelCatalog.Load(models, ModelCatalog.EmbeddingGemma)!;
        var img = new RgbImage(320, 180, Enumerable.Range(0, 320 * 180 * 3).Select(i => (byte)(i * 31 % 251)).ToArray());
        using var ort = new OnnxBackend(cpuOnly: true);
        using var ov = OpenVinoBackend.TryCreate()!;
        var results = new List<(string Dev, float[] Text, float[] LongText, float[] Image)>();
        foreach (var (backend, dev) in new (IInferenceBackend, DeviceInfo)[] { (ort, ort.Devices[^1]) }.Concat(ov.Devices.Where(d => d.Kind != DeviceKind.Gpu).Select(d => ((IInferenceBackend)ov, d))))
        {
            using var e = new Embedder(backend, dev, m, 140, Path.Combine(home, "cache", dev.Id));
            e.LoadAll();
            results.Add(($"{backend.Name}/{dev.Id}", e.EmbedText("quarterly sales", TextKind.Query),
                e.EmbedText(string.Join(' ', Enumerable.Repeat("budget meeting", 20)), TextKind.Document), e.EmbedImage(img)));
        }
        var cpu = results[0];
        foreach (var r in results)
        {
            double c = new[] { VectorMath.Cosine(r.Text, cpu.Text), VectorMath.Cosine(r.LongText, cpu.LongText), VectorMath.Cosine(r.Image, cpu.Image) }.Min();
            output.WriteLine($"{r.Dev}: min cosine vs ORT CPU {c:F6}");
            Assert.True(c > 0.999, $"{r.Dev} cosine {c}");
        }
        Assert.True(VectorMath.Cosine(cpu.Text, cpu.LongText) < 0.999);
    }

    /// <summary>Runs the full device benchmark through OpenVINO with inference.device=auto: NPU when present, CPU reference.</summary>
    [Fact]
    public void OpenVino_auto_benchmark_uses_the_npu_when_present()
    {
        var ov = OpenVinoBackend.TryCreate();
        Assert.NotNull(ov);
        var (home, models) = Home("ovnpu");
        TestEnv.InstallEmbeddingGemma(models);
        var s = TestEnv.Settings(home);
        using var engine = new RealEngine(s, home, ov!, new OnnxBackend(cpuOnly: true));
        var bench = engine.Bench();
        var lines = new List<string>
        {
            $"OpenVINO {ov!.Version}; NPU driver '{ov.NpuDriverVersion}'; devices: {string.Join(", ", ov.Devices.Select(d => $"{d.Id} ({d.Name})"))}",
            $"chosen {bench.Chosen}",
        };
        lines.AddRange(bench.Devices.Select(d => $"  {d.Device} {d.Ep} ok={d.Ok} image={d.ImageMs}ms text={d.TextMs}ms cos={d.Cosine} err={d.Error}"));
        foreach (var l in lines) output.WriteLine(l);
        File.WriteAllLines(Path.Combine(TestEnv.Root, "npu_synthetic_report.txt"), lines);

        bool hasNpu = ov.Devices.Any(d => d.Kind == DeviceKind.Npu);
        Assert.Contains(bench.Devices, d => d.Device == "CPU" && d.Ok);
        if (hasNpu)
        {
            var npu = Assert.Single(bench.Devices, d => d.Device == "NPU");
            Assert.True(npu.Ok, npu.Error);
            Assert.Equal("NPU", bench.Chosen);
            Assert.True(Directory.EnumerateFiles(BetterRecall.Contracts.DataPaths.CompiledCache(home), "*", SearchOption.AllDirectories).Any(),
                "NPU compiled blobs cached");
            var v = engine.EmbedImage(new ImageInput("bgra8", 320, 180, 1280, TestEnv.Bgra(320, 180, 40, 90, 160)), null).Vector!;
            Assert.False(VectorMath.HasNonFinite(v));
        }
        Assert.DoesNotContain(bench.Devices, d => d.Device == "GPU");
    }
}
