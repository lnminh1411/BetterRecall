// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
namespace BetterRecall.Worker.Inference;

/// <summary>Which GPU the embedder uses when inference.device is "gpu" (setting inference.gpuAdapter), and the GPU
/// list of the benchmark. Pure: the engine passes the main backend's devices and the DXGI adapter list.
/// OpenVINO GPUs (Intel only) come from the main backend; every other hardware DXGI adapter is a DirectML device
/// ("DML:&lt;EnumAdapters1 index&gt;") on ONNX Runtime. An Intel adapter is the same physical device as an OpenVINO
/// GPU, and OpenVINO wins for it.</summary>
public static class GpuSelection
{
    public static bool IsAuto(string? setting)
        => string.IsNullOrWhiteSpace(setting) || string.Equals(setting.Trim(), "auto", StringComparison.OrdinalIgnoreCase);

    /// <summary>The GPU "auto" means (today's rule): the main backend's first GPU, i.e. the OpenVINO Intel GPU, or
    /// DirectML's default adapter when ONNX Runtime is the main backend. Null when there is none.</summary>
    public static DeviceInfo? Auto(IReadOnlyList<DeviceInfo> main) => main.FirstOrDefault(d => d.Kind == DeviceKind.Gpu);

    public static DeviceInfo Dml(DxgiAdapter a) => new(DeviceKind.Gpu, $"DML:{a.Index}", a.Description, "DirectML");

    private static bool Hardware(DxgiAdapter a) => !a.IsSoftware && !a.IsBasicRender;

    /// <summary>The OpenVINO GPU that is the same physical device as an Intel DXGI adapter: integrated matches an
    /// integrated adapter (and the other way round); with one OpenVINO GPU, that one. Null for other vendors.</summary>
    public static DeviceInfo? OpenVinoTwin(DxgiAdapter a, IReadOnlyList<DeviceInfo> main)
    {
        if (a.VendorId != DxgiAdapter.VendorIntel) return null;
        var ov = main.Where(d => d.Kind == DeviceKind.Gpu && d.Ep == "OpenVINO").ToList();
        return ov.FirstOrDefault(d => d.Integrated == a.IsIntelIntegrated) ?? (ov.Count == 1 ? ov[0] : null);
    }

    /// <summary>Every GPU once: the main backend's OpenVINO GPUs, then a DirectML device per hardware adapter that
    /// has no OpenVINO twin (dml: the DirectML provider is present). Without a DXGI list the main backend's own
    /// DirectML device stays (ONNX Runtime main backend).</summary>
    public static IReadOnlyList<DeviceInfo> All(IReadOnlyList<DeviceInfo> main, IReadOnlyList<DxgiAdapter> adapters, bool dml)
    {
        var list = main.Where(d => d.Kind == DeviceKind.Gpu && d.Ep == "OpenVINO").ToList();
        if (dml)
            foreach (var a in adapters.Where(Hardware))
                if (OpenVinoTwin(a, main) is null) list.Add(Dml(a));
        if (!list.Any(d => d.Ep == "DirectML"))
            list.AddRange(main.Where(d => d.Kind == DeviceKind.Gpu && d.Ep == "DirectML"));
        return list;
    }

    /// <summary>inference.gpuAdapter to a device. auto: <see cref="Auto"/>. Otherwise a case-insensitive part of the
    /// name: an OpenVINO GPU whose FULL_DEVICE_NAME contains it; else a hardware DXGI adapter whose description
    /// contains it (its OpenVINO twin for an Intel adapter, else DirectML on that adapter). No match, or a DirectML
    /// adapter without the DirectML provider: <paramref name="warning"/> says why and the auto GPU is returned.</summary>
    public static DeviceInfo? Resolve(string? setting, IReadOnlyList<DeviceInfo> main, IReadOnlyList<DxgiAdapter> adapters, bool dml, out string? warning)
    {
        warning = null;
        var auto = Auto(main);
        if (IsAuto(setting)) return auto;
        var s = setting!.Trim();
        var ov = main.FirstOrDefault(d => d.Kind == DeviceKind.Gpu && d.Ep == "OpenVINO" && d.Name.Contains(s, StringComparison.OrdinalIgnoreCase));
        if (ov is not null) return ov;
        var a = adapters.FirstOrDefault(x => Hardware(x) && x.Description.Contains(s, StringComparison.OrdinalIgnoreCase));
        if (a is not null)
        {
            if (OpenVinoTwin(a, main) is { } twin) return twin;
            if (dml) return Dml(a);
            warning = $"inference.gpuAdapter '{s}' matches {a.Description}, but ONNX Runtime has no DirectML provider; using the automatic GPU";
            return auto;
        }
        warning = $"inference.gpuAdapter '{s}' matches no GPU; using the automatic GPU";
        return auto;
    }
}
