// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Diagnostics;
using System.Text.Json;
using BetterRecall.Contracts;
using BetterRecall.Worker.Hosting;
using BetterRecall.Worker.Inference;
using BetterRecall.Worker.Models;
using BetterRecall.Worker.Protocol;
using BetterRecall.Worker.Vectors;

namespace BetterRecall.Worker.Tests;

public class VectorTests
{
    [Fact]
    public void Matryoshka_truncates_and_renormalises()
    {
        var full = Enumerable.Range(1, 768).Select(i => (float)Math.Sin(i)).ToArray();
        VectorMath.L2NormalizeInPlace(full);
        foreach (var dim in new[] { 768, 512, 256 })
        {
            var v = VectorMath.Matryoshka(full, dim);
            Assert.Equal(dim, v.Length);
            Assert.Equal(1.0, Math.Sqrt(v.Sum(x => (double)x * x)), 5);
            // Direction of the prefix is kept.
            Assert.True(VectorMath.Cosine(v, full.AsSpan(0, dim)) > 0.99999);
        }
        Assert.Throws<ArgumentOutOfRangeException>(() => VectorMath.Matryoshka(full, 1024));
    }

    [Fact]
    public void Cosine_and_nan()
    {
        Assert.Equal(1.0, VectorMath.Cosine([1, 2, 3], [2, 4, 6]), 10);
        Assert.Equal(-1.0, VectorMath.Cosine([1, 0], [-1, 0]), 10);
        Assert.True(double.IsNaN(VectorMath.Cosine([1, float.NaN], [1, 1])));
        Assert.True(double.IsNaN(VectorMath.Cosine([0, 0], [1, 1])));
        Assert.True(VectorMath.HasNonFinite([1, float.PositiveInfinity]));
        Assert.False(VectorMath.HasNonFinite([1, 2]));
    }
}

public class NsfwGateTests
{
    private static readonly DeviceInfo Npu = new(DeviceKind.Npu, "NPU", "Intel(R) AI Boost", "OpenVINO");
    private static readonly DeviceInfo IGpu = new(DeviceKind.Gpu, "GPU.0", "Intel(R) Arc(TM) Graphics (iGPU)", "OpenVINO", Integrated: true);
    private static readonly DeviceInfo OvDGpu = new(DeviceKind.Gpu, "GPU.1", "Intel(R) Arc(TM) A770 (dGPU)", "OpenVINO", Integrated: false);
    private static readonly DeviceInfo Dml = new(DeviceKind.Gpu, "DML:1", "NVIDIA GeForce RTX 3050", "DirectML");
    private static readonly DeviceInfo OvCpu = new(DeviceKind.Cpu, "CPU", "Intel CPU", "OpenVINO");
    private static readonly DeviceInfo Cpu = new(DeviceKind.Cpu, "CPU", "CPU", "CPU");

    [Theory]
    [InlineData("auto", "NPU,GPU.0,DML:1,CPU")]
    [InlineData("npu", "NPU,CPU")]
    [InlineData("igpu", "GPU.0,CPU")]
    [InlineData("gpu", "DML:1,CPU")]
    [InlineData("cpu", "CPU")]
    [InlineData("bogus", "NPU,GPU.0,DML:1,CPU")]
    public void Device_order_all_present(string setting, string expected)
        => Assert.Equal(expected, string.Join(",", NsfwDevicePlan.Order(setting, [Cpu, Dml, OvDGpu, IGpu, Npu, OvCpu]).Select(d => d.Id)));

    [Theory]
    [InlineData("auto", "GPU.0,CPU")]      // no NPU, no DirectML adapter
    [InlineData("npu", "CPU")]
    [InlineData("gpu", "CPU")]
    [InlineData("igpu", "GPU.0,CPU")]
    public void Device_order_missing_devices(string setting, string expected)
        => Assert.Equal(expected, string.Join(",", NsfwDevicePlan.Order(setting, [IGpu, OvDGpu, Cpu]).Select(d => d.Id)));

    [Fact]
    public void Device_order_skips_unknown_gpu_type_and_openvino_cpu()
    {
        var unknown = new DeviceInfo(DeviceKind.Gpu, "GPU", "Intel(R) Graphics", "OpenVINO");
        Assert.Equal("CPU", string.Join(",", NsfwDevicePlan.Order("igpu", [unknown, Cpu]).Select(d => d.Ep)));
        Assert.Empty(NsfwDevicePlan.Order("cpu", [OvCpu]));
        Assert.Equal(["NPU", "iGPU", "GPU (NVIDIA GeForce RTX 3050)", "CPU"], new[] { Npu, IGpu, Dml, Cpu }.Select(NsfwDevicePlan.Label));
    }

    private static readonly DxgiAdapter Rx6600 = new(0, "AMD Radeon RX 6600", 8UL << 30, 0, 0x1002);
    private static readonly DxgiAdapter Rtx3050 = new(1, "NVIDIA GeForce RTX 3050", 6UL << 30, 0, 0x10DE);
    private static readonly DxgiAdapter IntelIgpu = new(2, "Intel(R) Arc(TM) Graphics", 128UL << 20, 0, 0x8086);
    private static readonly DxgiAdapter Basic = new(3, "Microsoft Basic Render Driver", 0, DxgiAdapter.FlagSoftware, 0x1414);

    [Theory]
    [InlineData("auto", 0)]
    [InlineData("AUTO", 0)]
    [InlineData("", 0)]
    [InlineData("rtx 3050", 1)]
    [InlineData("  RTX  ", 1)]
    [InlineData("Intel", 2)]
    [InlineData("Basic Render", 3)]
    [InlineData("Quadro", -1)]
    public void Dxgi_pick(string setting, int expected)
        => Assert.Equal(expected, DxgiAdapters.Pick(setting, [Rx6600, Rtx3050, IntelIgpu, Basic])?.Index ?? -1);

    [Fact]
    public void Dxgi_auto_skips_software_basic_and_intel_integrated()
    {
        Assert.Null(DxgiAdapters.Pick("auto", [IntelIgpu, Basic]));
        Assert.Equal(1, DxgiAdapters.Pick("auto", [IntelIgpu, Rtx3050, Rx6600])!.Index);
        var arc = new DxgiAdapter(4, "Intel(R) Arc(TM) A770 Graphics", 16UL << 30, 0, 0x8086);
        Assert.Equal(4, DxgiAdapters.Pick("auto", [IntelIgpu, arc])!.Index);   // discrete Intel has GBs of VRAM
        var warp = new DxgiAdapter(5, "Some software adapter", 0, DxgiAdapter.FlagSoftware, 0x1234);
        Assert.Null(DxgiAdapters.Pick("auto", [warp]));
    }

    [Theory]
    [InlineData("DML:0", 0)]
    [InlineData("DML:3", 3)]
    [InlineData("DML:x", 0)]
    [InlineData("DML:-1", 0)]
    [InlineData("CPU", 0)]
    public void Dml_device_id(string id, int expected) => Assert.Equal(expected, Inference.Onnx.OnnxBackend.DmlDeviceId(id));

    [Fact]
    public void Dxgi_enumerate_does_not_throw()
    {
        var list = DxgiAdapters.Enumerate();
        Assert.Equal(Enumerable.Range(0, list.Count), list.Select(a => a.Index));
    }

    [Fact]
    public void Block_score_is_max_of_block_rows()
    {
        // rows = 4 box + 3 labels, anchors = 4; label rows 4, 5, 6.
        float[] o =
        [
            900, 900, 900, 900,   // cx (large values must be ignored)
            900, 900, 900, 900,   // cy
            900, 900, 900, 900,   // w
            900, 900, 900, 900,   // h
            0.9f, 0.1f, 0.2f, 0.3f, // label 0 (not blocked)
            0.1f, 0.6f, 0.2f, 0.05f, // label 1
            0.2f, 0.3f, 0.7f, 0.1f,  // label 2
        ];
        Assert.Equal(0.6, NsfwDetector.BlockScore(o, 7, 4, [5]), 6);
        Assert.Equal(0.7, NsfwDetector.BlockScore(o, 7, 4, [5, 6]), 6);
        Assert.Equal(0.0, NsfwDetector.BlockScore(o, 7, 4, []), 6);
        Assert.Equal(0.6, NsfwDetector.BlockScore(o, 7, 4, [5, 99]), 6);     // out-of-range rows ignored
        o[4 * 4 + 4 + 2] = float.NaN;
        Assert.True(double.IsNaN(NsfwDetector.BlockScore(o, 7, 4, [5])));
        Assert.Throws<DeviceFailedException>(() => NsfwDetector.BlockScore(o.AsSpan(0, 10), 7, 4, [5]));
    }

    [Fact]
    public void Block_rows_follow_labels()
    {
        var spec = new DetectorSpec { Labels = [.. TestEnv.NudeNetLabels], BlockLabels = ["FEMALE_GENITALIA_COVERED", "BUTTOCKS_COVERED", "NOPE"] };
        Assert.Equal([4, 21], NsfwDetector.BlockRows(spec));
    }

    [Fact]
    public void Letterbox_pads_bottom_and_right_with_black()
    {
        // 4x2 white-ish image -> 4x4 canvas (rows 2-3 black) at native size: exact /255 values.
        var wide = new RgbImage(4, 2, Enumerable.Repeat((byte)200, 4 * 2 * 3).ToArray());
        var t = NsfwDetector.Letterbox(wide, 4);
        Assert.Equal(3 * 16, t.Length);
        for (int c = 0; c < 3; c++)
            for (int y = 0; y < 4; y++)
                for (int x = 0; x < 4; x++)
                    Assert.Equal(y < 2 ? 200f / 255f : 0f, t[c * 16 + y * 4 + x], 5);

        // 2x4 tall image: black columns on the right.
        var tall = new RgbImage(2, 4, Enumerable.Repeat((byte)255, 2 * 4 * 3).ToArray());
        t = NsfwDetector.Letterbox(tall, 4);
        for (int y = 0; y < 4; y++)
            for (int x = 0; x < 4; x++)
                Assert.Equal(x < 2 ? 1f : 0f, t[16 + y * 4 + x], 5);

        // 8x4 -> 8x8 canvas -> 4x4: first row is all image, last row all padding.
        var big = new RgbImage(8, 4, Enumerable.Repeat((byte)100, 8 * 4 * 3).ToArray());
        t = NsfwDetector.Letterbox(big, 4);
        for (int x = 0; x < 4; x++)
        {
            Assert.Equal(100f / 255f, t[x], 5);
            Assert.Equal(0f, t[3 * 4 + x], 5);
        }
    }

    [Fact]
    public void Classifier_file_per_device()
    {
        var spec = new ClassifierSpec { File = "model_int8.onnx", AcceleratorFile = "model_fp32.onnx" };
        Assert.Equal("model_int8.onnx", NsfwClassifier.FileFor(spec, DeviceKind.Cpu));
        Assert.Equal("model_fp32.onnx", NsfwClassifier.FileFor(spec, DeviceKind.Npu));
        Assert.Equal("model_fp32.onnx", NsfwClassifier.FileFor(spec, DeviceKind.Gpu));
        Assert.Equal("model_int8.onnx", NsfwClassifier.FileFor(new ClassifierSpec { File = "model_int8.onnx" }, DeviceKind.Npu));
    }

    [Fact]
    public void Worker_settings_nsfw_keys_fall_back_on_bad_values()
    {
        var s = new Settings();
        var w = WorkerSettings.From(s, @"Z:\no-such-dir");
        Assert.Equal("auto", w.NsfwDevice);
        Assert.Equal("auto", w.NsfwGpuAdapter);
        Assert.True(w.NsfwFilter && w.NsfwDetector);
        s.Privacy.NsfwDevice = " IGPU ";
        s.Privacy.NsfwGpuAdapter = " RTX 3050 ";
        s.Privacy.NsfwDetector = false;
        w = WorkerSettings.From(s, @"Z:\no-such-dir");
        Assert.Equal("igpu", w.NsfwDevice);
        Assert.Equal("RTX 3050", w.NsfwGpuAdapter);
        Assert.False(w.NsfwDetector);
        s.Privacy.NsfwDevice = "tpu";
        s.Privacy.NsfwGpuAdapter = "   ";
        w = WorkerSettings.From(s, @"Z:\no-such-dir");
        Assert.Equal("auto", w.NsfwDevice);
        Assert.Equal("auto", w.NsfwGpuAdapter);
    }

    [Fact]
    public void Manifest_detector_section_round_trips()
    {
        var json = """
            {"name":"nsfw-detector","detector":{"file":"model.onnx","input":"images","output":"output0","size":640,
             "labels":["A","B"],"blockLabels":["B"]},
             "classifier":{"file":"model_int8.onnx","acceleratorFile":"model_fp32.onnx"}}
            """u8;
        var m = JsonSerializer.Deserialize(json, WorkerJson.Default.ModelManifest)!;
        Assert.Equal(640, m.Detector!.Size);
        Assert.Equal(["B"], m.Detector.BlockLabels);
        Assert.Equal("model_fp32.onnx", m.Classifier!.AcceleratorFile);
        Assert.Null(JsonSerializer.Deserialize("""{"classifier":{"file":"x"}}"""u8, WorkerJson.Default.ModelManifest)!.Classifier!.AcceleratorFile);
    }

    [Fact]
    public void Manifest_onnx_map_picks_the_onnx_twin_only_on_onnx_runtime()
    {
        var m = JsonSerializer.Deserialize("""{"name":"embeddinggemma-2","onnx":{"text.xml":"text.onnx","vision_70.xml":"vision.onnx"}}"""u8,
            WorkerJson.Default.ModelManifest)!;
        Assert.Equal("text.onnx", m.FileFor("text.xml", onnxRuntime: true));
        Assert.Equal("vision.onnx", m.FileFor("vision_70.xml", onnxRuntime: true));
        Assert.Equal("text.xml", m.FileFor("text.xml", onnxRuntime: false));
        Assert.Equal("vision_140.xml", m.FileFor("vision_140.xml", onnxRuntime: true));   // no twin: unchanged
        Assert.Equal("text.xml", new ModelManifest().FileFor("text.xml", onnxRuntime: true));
    }
}

public class DevicePolicyTests
{
    private static readonly DeviceInfo Npu = new(DeviceKind.Npu, "NPU", "Intel(R) AI Boost", "OpenVINO");
    private static readonly DeviceInfo Gpu = new(DeviceKind.Gpu, "GPU.0", "Intel(R) Graphics", "OpenVINO");
    private static readonly DeviceInfo Cpu = new(DeviceKind.Cpu, "CPU", "CPU", "OpenVINO");

    [Theory]
    [InlineData("auto", true, true, "NPU,CPU")]
    [InlineData("auto", false, true, "GPU.0,CPU")]
    [InlineData("auto", false, false, "CPU")]
    [InlineData("npu", true, true, "NPU,CPU")]
    [InlineData("npu", false, true, "CPU")]
    [InlineData("gpu", true, true, "GPU.0,CPU")]
    [InlineData("cpu", true, true, "CPU")]
    public void Allowed_devices(string setting, bool npu, bool gpu, string expected)
    {
        var avail = new List<DeviceInfo>();
        if (npu) avail.Add(Npu);
        if (gpu) avail.Add(Gpu);
        avail.Add(Cpu);
        Assert.Equal(expected, string.Join(",", DevicePolicy.Allowed(setting, avail).Select(d => d.Id)));
    }

    [Theory]
    [InlineData(0.999, false, true)]
    [InlineData(0.9999, false, true)]
    [InlineData(0.9989, false, false)]
    [InlineData(1.0, true, false)]
    [InlineData(double.NaN, false, false)]
    [InlineData(0.67, false, false)]
    public void Accept_rejects_low_cosine_and_nan(double cos, bool nan, bool ok)
        => Assert.Equal(ok, DevicePolicy.Accept(cos, nan));

    [Fact]
    public void Choose_fastest_passing_primary()
    {
        var o = new List<BenchOutcome>
        {
            new(Cpu, "embeddinggemma-2", true, 2700, 60, 1, null),
            new(Npu, "embeddinggemma-2", true, 1300, 37, 0.9995, null),
        };
        Assert.Same(o[1], DevicePolicy.Choose(o, "embeddinggemma-2", "siglip2-base"));
        // A passing NPU wins even when the CPU is faster (section 1: keep the CPU free).
        o[0] = o[0] with { ImageMs = 10, TextMs = 1 };
        Assert.Same(o[1], DevicePolicy.Choose(o, "embeddinggemma-2", "siglip2-base"));
        // Among accelerators the fastest wins.
        var gpu = new BenchOutcome(Gpu, "embeddinggemma-2", true, 400, 10, 0.9999, null);
        Assert.Same(gpu, DevicePolicy.Choose([.. o, gpu], "embeddinggemma-2", "siglip2-base"));
        o[1] = o[1] with { Ok = false, Cosine = 0.67 };
        Assert.Same(o[0], DevicePolicy.Choose(o, "embeddinggemma-2", "siglip2-base"));
    }

    [Fact]
    public void Split_beats_siglip_and_cpu_but_not_a_full_npu_pass()
    {
        var full = new BenchOutcome(Npu, "embeddinggemma-2", false, 0, 0, 0.81, "cosine 0.8100 below 0.999", 0, Npu);
        var split = new BenchOutcome(Npu, "embeddinggemma-2", true, 1400, 30, 0.9999, null, 0, Cpu);
        var cpu = new BenchOutcome(Cpu, "embeddinggemma-2", true, 1300, 30, 1, null, 0, Cpu);
        var sig = new BenchOutcome(Npu, "siglip2-base", true, 50, 5, 0.9999, null, 0, Npu);
        Assert.True(split.Split);
        Assert.False(full.Split);
        Assert.Same(split, DevicePolicy.Choose([cpu, full, split, sig], "embeddinggemma-2", "siglip2-base"));
        var fullOk = full with { Ok = true, Cosine = 0.9995, ImageMs = 1500, Error = null };
        Assert.Same(fullOk, DevicePolicy.Choose([cpu, fullOk, split], "embeddinggemma-2", "siglip2-base"));
        Assert.Same(sig, DevicePolicy.Choose([cpu, full, split with { Ok = false }, sig], "embeddinggemma-2", "siglip2-base"));
    }

    [Fact]
    public void Choose_siglip_on_npu_when_primary_vision_fails_there()
    {
        var o = new List<BenchOutcome>
        {
            new(Cpu, "embeddinggemma-2", true, 2700, 60, 1, null),
            new(Npu, "embeddinggemma-2", false, 0, 0, double.NaN, "NPU compile failed: ConvertReduceToPooling"),
            new(Cpu, "siglip2-base", true, 300, 20, 1, null),
            new(Npu, "siglip2-base", true, 400, 15, 0.9997, null),
        };
        Assert.Same(o[3], DevicePolicy.Choose(o, "embeddinggemma-2", "siglip2-base"));
        o[3] = o[3] with { Ok = false };
        Assert.Same(o[0], DevicePolicy.Choose(o, "embeddinggemma-2", "siglip2-base"));
        Assert.Null(DevicePolicy.Choose([o[1], o[3]], "embeddinggemma-2", "siglip2-base"));
    }
}

public class DeviceCacheTests
{
    private static readonly DeviceCacheKey Key = new("OpenVINO", "2026.4.1-22982", "1005540", "abc+def", "auto", 140);

    [Fact]
    public void Roundtrip_and_invalidation()
    {
        var dir = TestEnv.NewDir("devcache");
        var path = Path.Combine(dir, "cache", "device.json");
        Assert.Null(DeviceCache.Read(path, Key));
        DeviceCache.Write(path, new DeviceCacheFile
        {
            Key = Key, Chosen = new DeviceChoice("NPU", "NPU", "OpenVINO", "embeddinggemma-2"),
            Bench = new BenchResult("NPU", [new BenchDevice("NPU", "OpenVINO", true, 1, 2, 0.9995, null)]), WrittenUtc = DateTimeOffset.UtcNow,
        });
        var back = DeviceCache.Read(path, Key);
        Assert.NotNull(back);
        Assert.Equal("NPU", back!.Chosen!.Device);
        Assert.Equal(0.9995, back.Bench!.Devices[0].Cosine);

        Assert.Null(DeviceCache.Read(path, Key with { RuntimeVersion = "2026.5.0" }));
        Assert.Null(DeviceCache.Read(path, Key with { NpuDriver = "1005600" }));
        Assert.Null(DeviceCache.Read(path, Key with { ModelHash = "abc+zzz" }));
        Assert.Null(DeviceCache.Read(path, Key with { Setting = "cpu" }));
        Assert.Null(DeviceCache.Read(path, Key with { ImageTokens = 70 }));
        Assert.Null(DeviceCache.Read(path, Key with { Runtime = "ONNX Runtime" }));

        File.WriteAllText(path, "{ not json");
        Assert.Null(DeviceCache.Read(path, Key));
    }

    [Fact]
    public void Key_ids_change_with_every_part()
    {
        var ids = new[]
        {
            Key, Key with { RuntimeVersion = "x" }, Key with { NpuDriver = "x" }, Key with { ModelHash = "x" },
        }.Select(k => k.CompiledId).ToHashSet();
        Assert.Equal(4, ids.Count);
        Assert.Equal(Key.CompiledId, (Key with { Setting = "cpu", ImageTokens = 70 }).CompiledId);
        Assert.NotEqual(Key.Id, (Key with { Setting = "cpu" }).Id);
    }

    [Fact]
    public void Compiled_dir_prunes_stale_blobs_of_the_same_model()
    {
        var root = TestEnv.NewDir("compiled");
        var old = DeviceCache.CompiledDir(root, "embeddinggemma-2", "1111111111111111");
        File.WriteAllText(Path.Combine(old, "blob"), "x");
        var other = DeviceCache.CompiledDir(root, "whisper-large-v3-turbo", "3333333333333333");
        var now = DeviceCache.CompiledDir(root, "embeddinggemma-2", "2222222222222222");
        Assert.True(Directory.Exists(now));
        Assert.False(Directory.Exists(old));
        Assert.True(Directory.Exists(other));
    }
}

public class SmallPartsTests
{
    [Fact]
    public void Tokenizer_matches_python_sentencepiece()
    {
        var spec = new TokenizerSpec { AddBos = false, AddEos = false };
        var tok = new GemmaTokenizer(TestEnv.Data("sp.model"), spec);
        using var doc = JsonDocument.Parse(File.ReadAllText(TestEnv.Data("sp_expected.json")));
        foreach (var e in doc.RootElement.EnumerateArray())
        {
            var text = e.GetProperty("text").GetString()!;
            var want = e.GetProperty("ids").EnumerateArray().Select(x => x.GetInt64()).ToArray();
            Assert.Equal(want, tok.Encode(text, 1000));
        }
    }

    /// <summary>Real Gemma tokenizer (google/embeddinggemma-2 tokenizer.model) against Python sentencepiece ids.
    /// Runs only when BR_GEMMA_TOKENIZER and BR_GEMMA_EXPECTED point at the files.</summary>
    [Fact]
    public void Real_gemma_tokenizer_matches_sentencepiece_when_available()
    {
        var model = Environment.GetEnvironmentVariable("BR_GEMMA_TOKENIZER");
        var expected = Environment.GetEnvironmentVariable("BR_GEMMA_EXPECTED");
        if (model is null || expected is null || !File.Exists(model) || !File.Exists(expected)) return;
        var tok = new GemmaTokenizer(model, new TokenizerSpec { AddBos = false, AddEos = false });
        using var doc = JsonDocument.Parse(File.ReadAllText(expected));
        foreach (var e in doc.RootElement.EnumerateArray())
        {
            var want = e.GetProperty("ids").EnumerateArray().Select(x => x.GetInt64()).ToArray();
            Assert.Equal(want, tok.Encode(e.GetProperty("text").GetString()!, 100000));
        }
    }

    [Fact]
    public void Tokenizer_adds_bos_eos_and_truncates()
    {
        var tok = new GemmaTokenizer(TestEnv.Data("sp.model"), new TokenizerSpec { AddBos = true, AddEos = true, BosId = 2, EosId = 1 });
        var ids = tok.Encode("title: none | text: a spreadsheet with quarterly sales figures and much more text", 8);
        Assert.Equal(8, ids.Length);
        Assert.Equal(2, ids[0]);
        Assert.Equal(1, ids[^1]);
    }

    [Fact]
    public void Resize_matches_pillow_within_one_level()
    {
        using var doc = JsonDocument.Parse(File.ReadAllText(TestEnv.Data("resize_ref.json")));
        var root = doc.RootElement;
        var src = new RgbImage(root.GetProperty("w").GetInt32(), root.GetProperty("h").GetInt32(),
            root.GetProperty("rgb").EnumerateArray().Select(x => (byte)x.GetInt32()).ToArray());
        foreach (var r in root.GetProperty("refs").EnumerateArray())
        {
            var want = r.GetProperty("rgb").EnumerateArray().Select(x => x.GetInt32()).ToArray();
            var got = ImagePipeline.Resize(src, r.GetProperty("w").GetInt32(), r.GetProperty("h").GetInt32(), r.GetProperty("mode").GetString()!);
            int maxDiff = want.Select((v, i) => Math.Abs(v - got.Rgb[i])).Max();
            Assert.True(maxDiff <= 1, $"{r.GetProperty("mode")} {r.GetProperty("w")}x{r.GetProperty("h")}: max diff {maxDiff}");
        }
    }

    [Fact]
    public void Decode_png_and_webp_through_skia()
    {
        using var bmp = new SkiaSharp.SKBitmap(new SkiaSharp.SKImageInfo(16, 8, SkiaSharp.SKColorType.Rgba8888, SkiaSharp.SKAlphaType.Unpremul));
        bmp.Erase(new SkiaSharp.SKColor(200, 100, 50));
        foreach (var (fmt, enc) in new[] { ("png", SkiaSharp.SKEncodedImageFormat.Png), ("webp", SkiaSharp.SKEncodedImageFormat.Webp), ("jpeg", SkiaSharp.SKEncodedImageFormat.Jpeg) })
        {
            using var data = bmp.Encode(enc, 100);
            var img = ImagePipeline.Decode(new Engine.ImageInput(fmt, 0, 0, 0, data.ToArray()));
            Assert.Equal(16, img.Width);
            Assert.InRange(img.Rgb[0], 195, 205);
            Assert.InRange(img.Rgb[1], 95, 105);
        }
        Assert.Throws<BadRequestException>(() => ImagePipeline.Decode(new Engine.ImageInput("png", 0, 0, 0, new byte[] { 1, 2, 3 })));
        using var png = bmp.Encode(SkiaSharp.SKEncodedImageFormat.Png, 100);
        Assert.Throws<BadRequestException>(() => ImagePipeline.Decode(new Engine.ImageInput("webp", 0, 0, 0, png.ToArray())));
    }

    [Fact]
    public void Nsfw_softmax()
    {
        Assert.Equal(0.5, Models.NsfwClassifier.BlockProbability([0f, 0f], [1]), 6);
        Assert.True(Models.NsfwClassifier.BlockProbability([0f, 10f], [1]) > 0.9999);
    }

    [Fact]
    public void Nsfw_grid2_quadrants_and_centre()
    {
        var t = Models.NsfwClassifier.Tiles(1920, 1080, 224, 224, 2).ToList();
        Assert.Equal([(0, 0, 960, 540), (960, 0, 960, 540), (0, 540, 960, 540), (960, 540, 960, 540), (480, 270, 960, 540)], t);
        Assert.All(Models.NsfwClassifier.Tiles(1281, 1025, 224, 224, 2), r =>   // odd sizes floor, stay inside
            Assert.True(r.W == 640 && r.H == 512 && r.X + r.W <= 1281 && r.Y + r.H <= 1025));
    }

    [Fact]
    public void Nsfw_grid3_ninths_inside_frame()
    {
        var t = Models.NsfwClassifier.Tiles(1920, 1080, 224, 224, 3).ToList();
        Assert.Equal(9, t.Count);
        Assert.Equal((1280, 720, 640, 360), t[^1]);
        Assert.Equal(9, t.Distinct().Count());
    }

    [Fact]
    public void Nsfw_grid_falls_back_on_small_frames()
    {
        Assert.Empty(Models.NsfwClassifier.Tiles(1920, 1080, 224, 224, 1));
        Assert.Equal(5, Models.NsfwClassifier.Tiles(600, 600, 224, 224, 3).Count());   // ninth 200 px < 224: grid 2
        Assert.Empty(Models.NsfwClassifier.Tiles(447, 447, 224, 224, 3));
        Assert.Empty(Models.NsfwClassifier.Tiles(400, 1080, 224, 224, 2));
        Assert.Equal(5, Models.NsfwClassifier.Tiles(448, 448, 224, 224, 2).Count());
    }

    [Fact]
    public void Crop_copies_rectangle()
    {
        var rgb = new byte[4 * 3 * 3];
        for (int i = 0; i < rgb.Length; i++) rgb[i] = (byte)i;
        var c = ImagePipeline.Crop(new RgbImage(4, 3, rgb), 1, 1, 2, 2);
        Assert.Equal(new byte[] { 15, 16, 17, 18, 19, 20, 27, 28, 29, 30, 31, 32 }, c.Rgb);
        Assert.Throws<ArgumentOutOfRangeException>(() => ImagePipeline.Crop(new RgbImage(4, 3, rgb), 3, 0, 2, 1));
    }

    [Fact]
    public void Args_and_pipe_names()
    {
        var name = PipeNames.NewWorker();
        Assert.Equal(name, AgentPipe.NormalizeName(@"\\.\pipe\" + name));
        Assert.Null(AgentPipe.NormalizeName("BetterRecall.ui.S-1-5"));
        Assert.Null(AgentPipe.NormalizeName("BetterRecall.worker.XYZ"));
        Assert.NotNull(WorkerArgs.Parse(["--pipe", name, "--home", @"C:\x"]));
        Assert.Null(WorkerArgs.Parse(["--pipe", name]));
        Assert.Null(WorkerArgs.Parse(["--pipe", "evil", "--home", @"C:\x"]));
        Assert.Null(WorkerArgs.Parse(["--pipe", name, "--home", @"C:\x", "--extra"]));
        Assert.Equal(@"C:\Prog\BR\BetterRecall.Agent.exe", AgentPipe.ExpectedAgentImage(@"C:\Prog\BR\worker\"));
    }

    [Fact]
    public void Settings_out_of_range_fall_back()
    {
        var s = new Settings();
        s.Index.Dim = 128; s.Inference.ImageTokens = 1120; s.Inference.Device = "TPU";
        var w = WorkerSettings.From(s, @"C:\home");
        Assert.Equal(768, w.Dim);
        Assert.Equal(140, w.ImageTokens);
        Assert.Equal("auto", w.Device);
        Assert.Equal(@"C:\home\models", w.ModelDir);
    }

    [Fact]
    public void Vad_segmenter_rules()
    {
        var home = TestEnv.NewDir("vad");
        TestEnv.InstallVad(Path.Combine(home, "models"));
        using var cpu = new Inference.Onnx.OnnxBackend(cpuOnly: true);
        using var vad = new Audio.SileroVad(cpu, ModelCatalog.VadPath(Path.Combine(home, "models"))!);
        // 1 s silence, 1 s tone, 1 s silence.
        var pcm = new float[48000];
        for (int i = 16000; i < 32000; i++) pcm[i] = 0.2f * MathF.Sin(i * 0.05f);
        var segs = vad.Segments(pcm);
        Assert.Single(segs);
        Assert.InRange(segs[0].StartMs, 900, 1000);
        Assert.InRange(segs[0].EndMs, 2000, 2100);
        Assert.Empty(vad.Segments(new float[16000]));
        // Too short to count (100 ms).
        var blip = new float[16000];
        for (int i = 8000; i < 9600; i++) blip[i] = 0.2f;
        Assert.Empty(vad.Segments(blip));
    }

    [Fact]
    public void Final_path_resolves_a_junction()
    {
        var dir = TestEnv.NewDir("junction");
        var real = Path.Combine(dir, "real");
        var link = Path.Combine(dir, "link");
        Directory.CreateDirectory(real);
        File.WriteAllBytes(Path.Combine(real, "a.exe"), [1, 2, 3]);

        using var proc = Process.Start(new ProcessStartInfo("cmd.exe", ["/c", "mklink", "/J", link, real]) { UseShellExecute = false, CreateNoWindow = true });
        proc?.WaitForExit();
        if (proc is null || proc.ExitCode != 0 || !Directory.Exists(link)) return;

        Assert.Equal(AgentPipe.FinalPath(Path.Combine(real, "a.exe")), AgentPipe.FinalPath(Path.Combine(link, "a.exe")), ignoreCase: true);
        Assert.False(AgentPipe.FinalPath(Path.Combine(real, "a.exe")).StartsWith(@"\\?\"));
        Assert.Equal(Path.GetFullPath(Path.Combine(dir, "missing.exe")), AgentPipe.FinalPath(Path.Combine(dir, "missing.exe")));
    }
}

public class PatchFormTests
{
    /// <summary>Expected values from transformers get_aspect_ratio_preserving_size (patch 16, pool 3).</summary>
    [Theory]
    [InlineData(720, 1280, 70, 288, 528)]
    [InlineData(720, 1280, 280, 576, 1056)]
    [InlineData(768, 1024, 140, 480, 624)]
    [InlineData(768, 1024, 280, 672, 912)]
    [InlineData(1080, 2560, 280, 480, 1200)]
    [InlineData(1920, 1080, 140, 720, 384)]
    [InlineData(10, 4000, 70, 48, 3360)]
    [InlineData(4000, 10, 280, 13440, 48)]
    [InlineData(48, 48, 70, 384, 384)]
    public void Aspect_size_matches_transformers(int h, int w, int budget, int eh, int ew)
        => Assert.Equal((eh, ew), ImagePipeline.AspectSize(h, w, 16, budget * 9, 3));

    [Fact]
    public void Patches_follow_the_gemma4_processor_layout()
    {
        // 528x288 is its own target size at budget 70 (as for a 16:9 frame), so no resize happens and pixels map 1:1
        // to patch elements.
        var v = new VisionGraph { Tokens = 70, PositionsInput = "position_ids", PoolInput = "pool_weights", Preprocess = new ImagePreprocessSpec { Mean = [0, 0, 0], Std = [1, 1, 1] } };
        const int h = 288, w = 528;
        Assert.Equal((h, w), ImagePipeline.AspectSize(h, w, 16, v.PatchBudget, 3));
        var rgb = new byte[w * h * 3];
        for (int y = 0; y < h; y++)
            for (int x = 0; x < w; x++)
                for (int c = 0; c < 3; c++) rgb[(y * w + x) * 3 + c] = (byte)((x + 2 * y + 50 * c) % 256);
        var (patches, pos, soft, ph, pw, pool) = ImagePipeline.ToPatches(new RgbImage(w, h, rgb), v);
        Assert.Equal((h, w), (ph, pw));
        int nh = h / 16, nw = w / 16;
        Assert.Equal(nh * nw / 9, soft);
        Assert.Equal(630 * 768, patches.Length);
        // patch (row 1, col 2): position (2, 1); element (dy 3, dx 5, channel 1) = pixel (y 19, x 37, c 1) / 255
        int idx = 1 * nw + 2;
        Assert.Equal(2, pos[idx * 2]);
        Assert.Equal(1, pos[idx * 2 + 1]);
        Assert.Equal(((37 + 2 * 19 + 50) % 256) / 255f, patches[idx * 768 + (3 * 16 + 5) * 3 + 1], 6);
        // padding: zero patches at (-1,-1)
        Assert.Equal(-1, pos[(nh * nw) * 2]);
        Assert.Equal(-1, pos[^1]);
        Assert.Equal(0f, patches[^1]);

        Assert.NotNull(pool);
        Assert.Equal(70 * 630, pool.Length);
        Assert.Equal(1f / 9f, pool[0 * 630 + idx], 6);
        int idx34 = 3 * nw + 4;
        int slot34 = 4 / 3 + (nw / 3) * (3 / 3);
        Assert.Equal(1f / 9f, pool[slot34 * 630 + idx34], 6);
        Assert.Equal(0f, pool[0 * 630 + idx34]);

        for (int pIdx = 0; pIdx < nh * nw; pIdx++)
        {
            float colSum = 0f;
            for (int s = 0; s < 70; s++) colSum += pool[s * 630 + pIdx];
            Assert.Equal(1f / 9f, colSum, 6);
        }
        int padIdx = nh * nw;
        float padColSum = 0f;
        for (int s = 0; s < 70; s++) padColSum += pool[s * 630 + padIdx];
        Assert.Equal(0f, padColSum);

        var nonzeroSlots = new HashSet<int>();
        for (int s = 0; s < 70; s++)
            for (int pIdx = 0; pIdx < 630; pIdx++)
                if (pool[s * 630 + pIdx] != 0f) { nonzeroSlots.Add(s); break; }
        Assert.Equal(soft, nonzeroSlots.Count);
    }

    [Fact]
    public void Bench_device_reports_model_split_and_compile_fields()
    {
        var npu = new DeviceInfo(DeviceKind.Npu, "NPU", "Intel(R) AI Boost", "OpenVINO");
        var cpu = new DeviceInfo(DeviceKind.Cpu, "CPU", "CPU", "OpenVINO");
        var split = BetterRecall.Worker.Engine.RealEngine.ToBenchDevice(new BenchOutcome(npu, "embeddinggemma-2", true, 480.04, 30, 0.99991, null, 41234.6, cpu));
        Assert.Equal(("NPU", "OpenVINO", "embeddinggemma-2", "CPU", 41235.0), (split.Device, split.Ep, split.Model, split.TextDevice, split.CompileMs!.Value));
        var cached = BetterRecall.Worker.Engine.RealEngine.ToBenchDevice(new BenchOutcome(npu, "siglip2-base", true, 50, 5, 1, null, 900, npu) { CompileCached = true });
        Assert.Null(cached.TextDevice);
        Assert.Null(cached.CompileMs);
        Assert.Equal("siglip2-base", cached.Model);
    }
}
