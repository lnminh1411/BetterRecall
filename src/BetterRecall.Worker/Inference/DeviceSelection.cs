// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Security.Cryptography;
using System.Text;
using System.Text.Json;
using BetterRecall.Contracts;
using BetterRecall.Worker.Hosting;
using BetterRecall.Worker.Models;

namespace BetterRecall.Worker.Inference;

/// <summary>Cache key of cache\device.json and cache\compiled\: runtime, runtime version, NPU driver version, model
/// hash (manifest SHA-256 of the primary and fallback embedders), plus the user's inference.device (with
/// inference.gpuAdapter, <see cref="SettingOf"/>) and image token budget. Any change invalidates the cached choice.</summary>
public sealed record DeviceCacheKey(string Runtime, string RuntimeVersion, string NpuDriver, string ModelHash, string Setting, int ImageTokens)
{
    public string Id => Convert.ToHexString(SHA256.HashData(Encoding.UTF8.GetBytes(
        $"{Runtime}|{RuntimeVersion}|{NpuDriver}|{ModelHash}|{Setting}|{ImageTokens}"))).ToLowerInvariant()[..16];

    /// <summary>Compiled blobs depend on runtime, driver and model only (not on the setting).</summary>
    public string CompiledId => Convert.ToHexString(SHA256.HashData(Encoding.UTF8.GetBytes(
        $"{Runtime}|{RuntimeVersion}|{NpuDriver}|{ModelHash}"))).ToLowerInvariant()[..16];

    /// <summary>Setting part of the key: inference.device, plus "|" and inference.gpuAdapter when that is not auto
    /// (so an auto adapter keeps the key caches written before the setting existed).</summary>
    public static string SettingOf(string device, string gpuAdapter)
        => GpuSelection.IsAuto(gpuAdapter) ? device : $"{device}|{gpuAdapter.Trim()}";
}

/// <summary>TextDeviceId is set for a split choice (vision tower on DeviceId, text transformer on TextDeviceId).</summary>
public sealed record DeviceChoice(string Device, string DeviceId, string Ep, string Model, string? TextDeviceId = null);

public sealed class DeviceCacheFile
{
    public DeviceCacheKey? Key { get; set; }
    public DeviceChoice? Chosen { get; set; }
    public BenchResult? Bench { get; set; }
    public DateTimeOffset WrittenUtc { get; set; }
}

public static class DeviceCache
{
    /// <summary>The cached choice when the file exists, parses and its key equals <paramref name="key"/>; else null.</summary>
    public static DeviceCacheFile? Read(string path, DeviceCacheKey key)
    {
        try
        {
            if (!File.Exists(path)) return null;
            var f = JsonSerializer.Deserialize(File.ReadAllBytes(path), WorkerJson.Default.DeviceCacheFile);
            if (f?.Key is null || f.Chosen is null) return null;
            return f.Key == key ? f : null;
        }
        catch (Exception e) when (e is JsonException or IOException or UnauthorizedAccessException)
        {
            return null;
        }
    }

    public static void Write(string path, DeviceCacheFile f)
    {
        Directory.CreateDirectory(Path.GetDirectoryName(path)!);
        var tmp = path + ".tmp";
        File.WriteAllBytes(tmp, JsonSerializer.SerializeToUtf8Bytes(f, WorkerJson.Default.DeviceCacheFile));
        File.Move(tmp, path, overwrite: true);
    }

    /// <summary>cache\compiled\&lt;id&gt;. Deletes sibling directories whose marker names the same model but another id
    /// (runtime, driver or model hash changed: those blobs can never load again).</summary>
    public static string CompiledDir(string compiledRoot, string modelName, string id)
    {
        var dir = Path.Combine(compiledRoot, $"{modelName}-{id}");
        try
        {
            if (Directory.Exists(compiledRoot))
                foreach (var d in Directory.GetDirectories(compiledRoot, modelName + "-*"))
                    if (!string.Equals(Path.GetFileName(d), Path.GetFileName(dir), StringComparison.OrdinalIgnoreCase))
                    {
                        try { Directory.Delete(d, recursive: true); Log.Info($"pruned stale compiled cache {Path.GetFileName(d)}"); }
                        catch (IOException) { }
                    }
            Directory.CreateDirectory(dir);
        }
        catch (Exception e) when (e is IOException or UnauthorizedAccessException) { Log.Warn($"compiled cache: {e.GetType().Name}"); }
        return dir;
    }
}

/// <summary>One device run in the benchmark.</summary>
public sealed record BenchOutcome(DeviceInfo Device, string Model, bool Ok, double ImageMs, double TextMs, double Cosine, string? Error,
    double CompileMs = 0, DeviceInfo? TextDevice = null)
{
    public bool Split => TextDevice is not null && TextDevice.Id != Device.Id;
    /// <summary>Every graph came from the compiled cache (CompileMs is then load time, not compile time).</summary>
    public bool CompileCached { get; init; }
}

/// <summary>Device policy of docs/v1-design.md section 4. Pure functions; the benchmark that feeds them lives in RealEngine.</summary>
public static class DevicePolicy
{
    public const double MinCosine = 0.999;

    /// <summary>Devices to try, CPU (the reference) always last. auto: NPU then CPU; a GPU only when no NPU exists.
    /// npu / gpu: that device (when present) then CPU. cpu: CPU only. gpuChoice: the GPU inference.gpuAdapter
    /// resolved to (<see cref="GpuSelection.Resolve"/>), used for "gpu"; null = the first GPU in the list.</summary>
    public static IReadOnlyList<DeviceInfo> Allowed(string setting, IReadOnlyList<DeviceInfo> available, DeviceInfo? gpuChoice = null)
    {
        var npu = available.FirstOrDefault(d => d.Kind == DeviceKind.Npu);
        var gpu = available.FirstOrDefault(d => d.Kind == DeviceKind.Gpu);
        if (setting == "gpu" && gpuChoice is not null) gpu = gpuChoice;
        var cpu = available.FirstOrDefault(d => d.Kind == DeviceKind.Cpu) ?? throw new InvalidOperationException("no CPU device");
        var list = new List<DeviceInfo>();
        switch (setting)
        {
            case "cpu": break;
            case "npu": if (npu is not null) list.Add(npu); break;
            case "gpu": if (gpu is not null) list.Add(gpu); break;
            default:
                if (npu is not null) list.Add(npu);
                else if (gpu is not null) list.Add(gpu);
                break;
        }
        list.Add(cpu);
        return list;
    }

    /// <summary>Devices the bench op measures: the allowed ones and, when the PC has more than one GPU
    /// (<see cref="GpuSelection.All"/>), every other GPU as a measure-only row that is never chosen.</summary>
    public static IReadOnlyList<DeviceInfo> BenchDevices(IReadOnlyList<DeviceInfo> allowed, IReadOnlyList<DeviceInfo> gpus)
        => gpus.Count > 1 ? [.. allowed, .. gpus.Where(g => !allowed.Any(a => a.Id == g.Id))] : allowed;

    /// <summary>True when the device output is usable: no NaN, cosine to the CPU reference at least 0.999.</summary>
    public static bool Accept(double cosine, bool outputHasNaN)
        => !outputHasNaN && !double.IsNaN(cosine) && cosine >= MinCosine;

    /// <summary>For the primary model: the fastest passing accelerator (NPU or GPU), else the CPU. v1-design section 1
    /// puts EmbeddingGemma 2 on the NPU to keep the CPU free, so a passing accelerator beats a faster CPU; "keep the
    /// fastest" (section 4) applies among accelerators, with the whole model on the accelerator ahead of a split
    /// (vision tower on the accelerator, text on the CPU). If the primary passed nowhere on the NPU and the fallback
    /// passed there, the fallback on the NPU wins. Null when nothing passed.</summary>
    public static BenchOutcome? Choose(IReadOnlyList<BenchOutcome> outcomes, string primary, string fallback)
    {
        bool primaryFailedOnNpu = outcomes.Any(o => o.Model == primary && o.Device.Kind == DeviceKind.Npu && !o.Ok)
                                  && !outcomes.Any(o => o.Model == primary && o.Device.Kind == DeviceKind.Npu && o.Ok);
        var fbNpu = outcomes.FirstOrDefault(o => o.Model == fallback && o.Device.Kind == DeviceKind.Npu && o.Ok);
        if (primaryFailedOnNpu && fbNpu is not null) return fbNpu;
        return Best(outcomes.Where(o => o.Model == primary && o.Ok)) ?? Best(outcomes.Where(o => o.Model == fallback && o.Ok));
    }

    private static BenchOutcome? Best(IEnumerable<BenchOutcome> passing)
        => passing.OrderBy(o => o.Device.Kind == DeviceKind.Cpu ? 1 : 0).ThenBy(o => o.Split ? 1 : 0)
                  .ThenBy(o => o.ImageMs + o.TextMs).FirstOrDefault();
}
