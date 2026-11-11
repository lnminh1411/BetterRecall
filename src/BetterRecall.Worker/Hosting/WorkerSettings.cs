// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Text.Json;
using BetterRecall.Contracts;

namespace BetterRecall.Worker.Hosting;

/// <summary>The worker's view of settings.json: inference.* (gpuAdapter included), index.dim, audio.language, governance.ecoQos,
/// security.memoryIsolation, log.level, and the NSFW gate's privacy.nsfwFilter / nsfwDetector / nsfwDevice /
/// nsfwGpuAdapter. Out-of-range values fall back to defaults (Core validates the file). Read once at start: a change
/// takes effect when the agent next starts the worker (after the idle exit, inference.idleExitS).</summary>
public sealed record WorkerSettings(
    bool Fake,
    string Device,          // auto | npu | gpu | cpu
    string ModelDir,        // resolved, absolute
    int ImageTokens,        // 70 | 140 | 280
    int IdleExitS,
    int Dim,                // 768 | 512 | 256
    string AudioLanguage,   // auto | code
    bool EcoQos,
    bool MemoryIsolation,
    string LogLevel)
{
    /// <summary>privacy.nsfwFilter: when on, hello loads the NSFW gate so its device is known up front.</summary>
    public bool NsfwFilter { get; init; } = true;
    /// <summary>privacy.nsfwDetector: hello also loads the detector (requests still say per call whether it runs).</summary>
    public bool NsfwDetector { get; init; } = true;
    /// <summary>privacy.nsfwDevice: auto | npu | igpu | gpu | cpu (<see cref="Inference.NsfwDevicePlan"/>).</summary>
    public string NsfwDevice { get; init; } = "auto";
    /// <summary>privacy.nsfwGpuAdapter: "auto" or part of a DXGI adapter description.</summary>
    public string NsfwGpuAdapter { get; init; } = "auto";
    /// <summary>inference.gpuAdapter: "auto" or part of a GPU name (<see cref="Inference.GpuSelection.Resolve"/>);
    /// used when Device is gpu.</summary>
    public string GpuAdapter { get; init; } = "auto";

    public static readonly int[] Dims = [768, 512, 256];
    public static readonly int[] TokenBudgets = [70, 140, 280];
    public static readonly string[] DeviceValues = ["auto", "npu", "gpu", "cpu"];

    public static WorkerSettings Load(string home)
    {
        Settings s;
        var file = DataPaths.SettingsFile(home);
        try
        {
            s = File.Exists(file)
                ? JsonSerializer.Deserialize(File.ReadAllBytes(file), ContractsJson.Default.Settings) ?? new Settings()
                : new Settings();
        }
        catch (Exception e) when (e is JsonException or IOException or UnauthorizedAccessException or NotSupportedException)
        {
            Log.Warn($"settings.json unreadable ({e.GetType().Name}); using defaults");
            s = new Settings();
        }
        return From(s, home);
    }

    public static WorkerSettings From(Settings s, string home)
    {
        var inf = s.Inference ?? new InferenceSettings();
        string device = (inf.Device ?? "auto").Trim().ToLowerInvariant();
        if (Array.IndexOf(DeviceValues, device) < 0) device = "auto";
        int tokens = Array.IndexOf(TokenBudgets, inf.ImageTokens) >= 0 ? inf.ImageTokens : 140;
        int dim = Array.IndexOf(Dims, s.Index?.Dim ?? 768) >= 0 ? s.Index!.Dim : 768;
        int idle = Math.Clamp(inf.IdleExitS, 10, 3600);
        string modelDir = Path.GetFullPath(DataPaths.ModelsDir(home, s));
        string lang = string.IsNullOrWhiteSpace(s.Audio?.Language) ? "auto" : s.Audio!.Language.Trim();
        var pv = s.Privacy ?? new PrivacySettings();
        string nsfwDevice = (pv.NsfwDevice ?? "auto").Trim().ToLowerInvariant();
        if (Array.IndexOf(Inference.NsfwDevicePlan.Values, nsfwDevice) < 0) nsfwDevice = "auto";
        string adapter = pv.NsfwGpuAdapter?.Trim() ?? "";
        if (adapter.Length is 0 or > 128) adapter = "auto";
        string gpuAdapter = inf.GpuAdapter?.Trim() ?? "";
        if (gpuAdapter.Length is 0 or > 128) gpuAdapter = "auto";
        return new WorkerSettings(
            inf.Fake, device, modelDir, tokens, idle, dim, lang,
            s.Governance?.EcoQos ?? true,
            s.Security?.MemoryIsolation ?? true,
            (s.Log?.Level ?? "info").ToLowerInvariant())
        {
            NsfwFilter = pv.NsfwFilter,
            NsfwDetector = pv.NsfwDetector,
            NsfwDevice = nsfwDevice,
            NsfwGpuAdapter = adapter,
            GpuAdapter = gpuAdapter,
        };
    }
}
