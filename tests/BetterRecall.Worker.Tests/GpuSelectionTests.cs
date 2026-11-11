// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using BetterRecall.Contracts;
using BetterRecall.Worker.Engine;
using BetterRecall.Worker.Hosting;
using BetterRecall.Worker.Inference;
using BetterRecall.Worker.Inference.Onnx;

namespace BetterRecall.Worker.Tests;

/// <summary>inference.gpuAdapter resolution and the per-GPU benchmark rows. Hardware as on the developer PC: Intel NPU,
/// Intel iGPU (OpenVINO GPU.0), DXGI 0 RX 6600, 1 RTX 3050, 2 Intel iGPU, 3 Basic Render Driver.</summary>
public class GpuSelectionTests
{
    private static readonly DeviceInfo Npu = new(DeviceKind.Npu, "NPU", "Intel(R) AI Boost", "OpenVINO");
    private static readonly DeviceInfo IGpu = new(DeviceKind.Gpu, "GPU.0", "Intel(R) Arc(TM) 140T GPU (16GB) (iGPU)", "OpenVINO", Integrated: true);
    private static readonly DeviceInfo OvCpu = new(DeviceKind.Cpu, "CPU", "Intel(R) Core(TM) Ultra 7", "OpenVINO");
    private static readonly IReadOnlyList<DeviceInfo> OvMain = [Npu, IGpu, OvCpu];

    private static readonly DxgiAdapter Rx6600 = new(0, "AMD Radeon RX 6600", 8UL << 30, 0, 0x1002);
    private static readonly DxgiAdapter Rtx3050 = new(1, "NVIDIA GeForce RTX 3050", 6UL << 30, 0, 0x10DE);
    private static readonly DxgiAdapter IntelIgpu = new(2, "Intel(R) Arc(TM) 140T GPU (16GB)", 128UL << 20, 0, 0x8086);
    private static readonly DxgiAdapter Basic = new(3, "Microsoft Basic Render Driver", 0, DxgiAdapter.FlagSoftware, 0x1414);
    private static readonly IReadOnlyList<DxgiAdapter> Dxgi = [Rx6600, Rtx3050, IntelIgpu, Basic];

    private static string Id(DeviceInfo? d) => d is null ? "none" : $"{d.Ep}:{d.Id}";

    [Theory]
    [InlineData("auto", "OpenVINO:GPU.0")]
    [InlineData("AUTO", "OpenVINO:GPU.0")]
    [InlineData("", "OpenVINO:GPU.0")]
    [InlineData("Intel", "OpenVINO:GPU.0")]              // OpenVINO FULL_DEVICE_NAME first
    [InlineData("iGPU", "OpenVINO:GPU.0")]
    [InlineData("rtx 3050", "DirectML:DML:1")]
    [InlineData(" RX 6600 ", "DirectML:DML:0")]
    [InlineData("NVIDIA GeForce RTX 3050", "DirectML:DML:1")]
    public void Resolve_matches_openvino_then_dxgi(string setting, string expected)
    {
        Assert.Equal(expected, Id(GpuSelection.Resolve(setting, OvMain, Dxgi, dml: true, out var warning)));
        Assert.Null(warning);
    }

    [Theory]
    [InlineData("Quadro")]
    [InlineData("Basic Render")]   // software adapter: never a GPU to pick
    public void Resolve_no_match_warns_and_falls_back_to_auto(string setting)
    {
        Assert.Equal("OpenVINO:GPU.0", Id(GpuSelection.Resolve(setting, OvMain, Dxgi, dml: true, out var warning)));
        Assert.Contains("matches no GPU", warning);
    }

    [Fact]
    public void Resolve_edge_cases()
    {
        // An Intel DXGI name that is not part of the OpenVINO name still lands on the OpenVINO twin.
        var arc = new DxgiAdapter(2, "Intel(R) Graphics Family", 128UL << 20, 0, 0x8086);
        Assert.Equal("OpenVINO:GPU.0", Id(GpuSelection.Resolve("Graphics Family", OvMain, [Rx6600, arc], true, out var w1)));
        Assert.Null(w1);
        // DirectML provider missing: warning, auto.
        Assert.Equal("OpenVINO:GPU.0", Id(GpuSelection.Resolve("RTX 3050", OvMain, Dxgi, dml: false, out var w2)));
        Assert.Contains("no DirectML provider", w2);
        // ONNX Runtime main backend (no OpenVINO): auto is its default DirectML device, a name picks the adapter.
        IReadOnlyList<DeviceInfo> ortMain = [new(DeviceKind.Gpu, "DML:0", "DirectML default adapter", "DirectML"), new(DeviceKind.Cpu, "CPU", "CPU", "CPU")];
        Assert.Equal("DirectML:DML:0", Id(GpuSelection.Resolve("auto", ortMain, Dxgi, true, out _)));
        Assert.Equal("DirectML:DML:1", Id(GpuSelection.Resolve("3050", ortMain, Dxgi, true, out _)));
        // No GPU anywhere.
        Assert.Null(GpuSelection.Resolve("auto", [OvCpu], [], true, out _));
        Assert.Null(GpuSelection.Resolve("RTX", [OvCpu], [], true, out var w3));
        Assert.NotNull(w3);
        // The resolved device carries the adapter's name.
        Assert.Equal("NVIDIA GeForce RTX 3050", GpuSelection.Resolve("3050", OvMain, Dxgi, true, out _)!.Name);
    }

    [Fact]
    public void All_lists_every_gpu_once_preferring_openvino_for_intel()
    {
        Assert.Equal("OpenVINO:GPU.0,DirectML:DML:0,DirectML:DML:1", string.Join(",", GpuSelection.All(OvMain, Dxgi, true).Select(Id)));
        Assert.Equal("OpenVINO:GPU.0", string.Join(",", GpuSelection.All(OvMain, Dxgi, false).Select(Id)));
        IReadOnlyList<DeviceInfo> ortMain = [new(DeviceKind.Gpu, "DML:0", "DirectML default adapter", "DirectML"), new(DeviceKind.Cpu, "CPU", "CPU", "CPU")];
        Assert.Equal("DirectML:DML:0", string.Join(",", GpuSelection.All(ortMain, [], true).Select(Id)));   // no DXGI list
        Assert.Equal("DirectML:DML:0,DirectML:DML:1,DirectML:DML:2", string.Join(",", GpuSelection.All(ortMain, Dxgi, true).Select(Id)));
    }

    [Fact]
    public void Allowed_uses_the_resolved_gpu_only_for_gpu()
    {
        var rtx = GpuSelection.Dml(Rtx3050);
        Assert.Equal("DML:1,CPU", string.Join(",", DevicePolicy.Allowed("gpu", OvMain, rtx).Select(d => d.Id)));
        Assert.Equal("GPU.0,CPU", string.Join(",", DevicePolicy.Allowed("gpu", OvMain).Select(d => d.Id)));
        Assert.Equal("NPU,CPU", string.Join(",", DevicePolicy.Allowed("auto", OvMain, rtx).Select(d => d.Id)));
        Assert.Equal("CPU", string.Join(",", DevicePolicy.Allowed("cpu", OvMain, rtx).Select(d => d.Id)));
    }

    [Fact]
    public void Bench_devices_add_every_other_gpu_only_with_several_gpus()
    {
        var gpus = GpuSelection.All(OvMain, Dxgi, true);
        var allowed = DevicePolicy.Allowed("auto", OvMain);
        Assert.Equal("NPU,CPU,GPU.0,DML:0,DML:1", string.Join(",", DevicePolicy.BenchDevices(allowed, gpus).Select(d => d.Id)));
        var gpuAllowed = DevicePolicy.Allowed("gpu", OvMain, gpus[2]);
        Assert.Equal("DML:1,CPU,GPU.0,DML:0", string.Join(",", DevicePolicy.BenchDevices(gpuAllowed, gpus).Select(d => d.Id)));
        // One GPU: today's list, unchanged.
        Assert.Same(allowed, DevicePolicy.BenchDevices(allowed, [IGpu]));
        Assert.Same(allowed, DevicePolicy.BenchDevices(allowed, []));
    }

    [Fact]
    public void Bench_rows_carry_adapter_names_with_several_gpus()
    {
        var gpus = GpuSelection.All(OvMain, Dxgi, true);
        var rtx = RealEngine.ToBenchDevice(new BenchOutcome(gpus[2], "embeddinggemma-2", true, 90, 9, 0.9999, null), gpus, IGpu);
        Assert.Equal(("GPU", "DirectML", "NVIDIA GeForce RTX 3050", false), (rtx.Device, rtx.Ep, rtx.Adapter, rtx.AdapterAuto));
        var igpu = RealEngine.ToBenchDevice(new BenchOutcome(IGpu, "embeddinggemma-2", true, 300, 9, 0.9999, null), gpus, IGpu);
        Assert.Equal((IGpu.Name, true), (igpu.Adapter, igpu.AdapterAuto));
        var npu = RealEngine.ToBenchDevice(new BenchOutcome(Npu, "embeddinggemma-2", true, 500, 9, 0.9999, null), gpus, IGpu);
        Assert.Null(npu.Adapter);
        // Single GPU: no adapter fields (today's rows).
        var single = RealEngine.ToBenchDevice(new BenchOutcome(IGpu, "embeddinggemma-2", true, 300, 9, 0.9999, null), [IGpu], IGpu);
        Assert.Null(single.Adapter);
        Assert.False(single.AdapterAuto);
    }

    [Fact]
    public void Cache_key_setting_includes_a_non_auto_adapter()
    {
        Assert.Equal("gpu", DeviceCacheKey.SettingOf("gpu", "auto"));
        Assert.Equal("auto", DeviceCacheKey.SettingOf("auto", "AUTO"));
        Assert.Equal("gpu|RTX 3050", DeviceCacheKey.SettingOf("gpu", " RTX 3050 "));
        var k = new DeviceCacheKey("OpenVINO", "1", "2", "h", DeviceCacheKey.SettingOf("gpu", "auto"), 140);
        Assert.NotEqual(k.Id, (k with { Setting = DeviceCacheKey.SettingOf("gpu", "RTX 3050") }).Id);
        Assert.NotEqual((k with { Setting = DeviceCacheKey.SettingOf("gpu", "RX 6600") }).Id, (k with { Setting = DeviceCacheKey.SettingOf("gpu", "RTX 3050") }).Id);
    }

    [Fact]
    public void Worker_settings_gpu_adapter_falls_back_on_bad_values()
    {
        var s = new Settings();
        Assert.Equal("auto", WorkerSettings.From(s, @"Z:\no-such-dir").GpuAdapter);
        s.Inference.GpuAdapter = " RTX 3050 ";
        Assert.Equal("RTX 3050", WorkerSettings.From(s, @"Z:\no-such-dir").GpuAdapter);
        s.Inference.GpuAdapter = "   ";
        Assert.Equal("auto", WorkerSettings.From(s, @"Z:\no-such-dir").GpuAdapter);
        s.Inference.GpuAdapter = new string('a', 129);
        Assert.Equal("auto", WorkerSettings.From(s, @"Z:\no-such-dir").GpuAdapter);
    }

    [Fact]
    public void Onnx_runtime_refuses_openvino_ir_with_a_clear_message()
    {
        using var ort = new OnnxBackend(cpuOnly: true);
        var e = Assert.Throws<Protocol.DeviceFailedException>(() => ort.Load(@"Z:\no-such-dir\text.xml", ort.Devices[^1], null));
        Assert.Contains("OpenVINO IR", e.Message);
    }

    /// <summary>Main backend with two GPUs whose graphs actually run on the ONNX Runtime CPU (synthetic models).</summary>
    private sealed class TwoGpuBackend : IInferenceBackend
    {
        private readonly OnnxBackend _cpu = new(cpuOnly: true);
        public string Name => "Fake";
        public string Version => "1";
        public string NpuDriverVersion => "";
        public IReadOnlyList<DeviceInfo> Devices { get; } =
        [
            new(DeviceKind.Gpu, "GPU.0", "Fake iGPU", "OpenVINO", Integrated: true),
            new(DeviceKind.Gpu, "GPU.1", "Fake Arc dGPU", "OpenVINO", Integrated: false),
            new(DeviceKind.Cpu, "CPU", "CPU", "OpenVINO"),
        ];
        public IInferenceSession Load(string modelPath, DeviceInfo device, string? cacheDir, IReadOnlyDictionary<string, long[]>? staticShapes = null)
            => _cpu.Load(modelPath, _cpu.Devices[^1], null);
        public void Dispose() => _cpu.Dispose();
    }

    [Theory]
    [InlineData("auto", "GPU.0", "Fake iGPU")]
    [InlineData("arc", "GPU.1", "Fake Arc dGPU")]
    [InlineData("nothing like it", "GPU.0", "Fake iGPU")]
    public void Engine_bench_measures_each_gpu_and_picks_the_chosen_adapter(string adapter, string expectedId, string expectedName)
    {
        var home = TestEnv.NewDir("multigpu");
        TestEnv.InstallEmbeddingGemma(Path.Combine(home, "models"));
        var s = TestEnv.Settings(home, x => { x.Inference.Device = "gpu"; x.Inference.GpuAdapter = adapter; });
        using var engine = new RealEngine(s, home, new TwoGpuBackend(), new OnnxBackend(cpuOnly: true));
        var r = engine.Bench();
        var gpuRows = r.Devices.Where(d => d.Device == "GPU").ToList();
        Assert.Equal(["Fake Arc dGPU", "Fake iGPU"], gpuRows.Select(d => d.Adapter!).Order(StringComparer.Ordinal));
        Assert.True(gpuRows.All(d => d.Ok));
        Assert.Equal([true, false], gpuRows.OrderBy(d => d.Adapter == "Fake iGPU" ? 0 : 1).Select(d => d.AdapterAuto));
        Assert.Equal("GPU", r.Chosen);
        Assert.Equal(expectedName, r.ChosenAdapter);
        Assert.Single(r.Devices, d => d.Device == "CPU");
        var file = System.Text.Json.JsonSerializer.Deserialize(File.ReadAllBytes(DataPaths.DeviceCache(home)), Models.WorkerJson.Default.DeviceCacheFile)!;
        Assert.Equal(expectedId, file.Chosen!.DeviceId);
        Assert.Equal(DeviceCacheKey.SettingOf("gpu", adapter), file.Key!.Setting);
        // Token budgets are timed on the chosen GPU only.
        Assert.All(r.Tokens ?? [], t => Assert.Equal("GPU", t.Device));
    }

    [Fact]
    public void Engine_bench_with_one_gpu_keeps_todays_rows()
    {
        var home = TestEnv.NewDir("onegpu");
        TestEnv.InstallEmbeddingGemma(Path.Combine(home, "models"));
        var s = TestEnv.Settings(home, x => x.Inference.Device = "cpu");
        using var engine = new RealEngine(s, home, new OnnxBackend(cpuOnly: true), new OnnxBackend(cpuOnly: true));
        var r = engine.Bench();
        Assert.All(r.Devices, d => Assert.Null(d.Adapter));
        Assert.Null(r.ChosenAdapter);
        Assert.Equal("CPU", r.Chosen);
    }
}
