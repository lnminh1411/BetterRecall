// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Text.Json;
using System.Text.Json.Nodes;
using BetterRecall.Contracts;

namespace BetterRecall.Agent.Worker;

/// <summary>
/// Settings the worker reads once at start (BetterRecall.Worker WorkerSettings.From, plus governance.ecoQos which the
/// launcher applies to the process). A change to any of them needs a worker restart to take effect (S4).
/// </summary>
internal static class WorkerSettingsDiff
{
    /// <summary>The first worker-side key that differs, or null when the running worker is still current. Pure.</summary>
    public static string? Changed(Settings a, Settings b)
    {
        foreach (var (key, get) in Keys)
            if (!string.Equals(get(a), get(b), StringComparison.Ordinal)) return key;
        return null;
    }

    /// <summary>The worker keys of s as a partial settings.json object: the worker's --settings argument, so it runs on the
    /// agent's accepted settings and never reads settings.json, whose hand edits the agent has not loaded (sweep 4 C5).</summary>
    public static string Json(Settings s)
    {
        var full = JsonSerializer.SerializeToNode(s, ContractsJson.Default.Settings) as JsonObject ?? [];
        var o = new JsonObject();
        foreach (var (key, _) in Keys)
        {
            int dot = key.IndexOf('.');
            string section = key[..dot], name = key[(dot + 1)..];
            if (full[section] is not JsonObject src || !src.TryGetPropertyValue(name, out var value)) continue;
            if (o[section] is not JsonObject dst) o[section] = dst = new JsonObject();
            dst[name] = value?.DeepClone();
        }
        return o.ToJsonString();
    }

    private static readonly (string Key, Func<Settings, string?> Get)[] Keys =
    [
        ("inference.device", s => s.Inference?.Device),
        ("inference.gpuAdapter", s => s.Inference?.GpuAdapter),
        ("inference.modelDir", s => s.Inference?.ModelDir),
        ("inference.imageTokens", s => s.Inference?.ImageTokens.ToString()),
        ("inference.idleExitS", s => s.Inference?.IdleExitS.ToString()),
        ("inference.fake", s => s.Inference?.Fake.ToString()),
        ("index.dim", s => s.Index?.Dim.ToString()),
        ("audio.language", s => s.Audio?.Language),
        ("governance.ecoQos", s => s.Governance?.EcoQos.ToString()),
        ("security.memoryIsolation", s => s.Security?.MemoryIsolation.ToString()),
        ("log.level", s => s.Log?.Level),
        ("privacy.nsfwFilter", s => s.Privacy?.NsfwFilter.ToString()),
        ("privacy.nsfwDetector", s => s.Privacy?.NsfwDetector.ToString()),
        ("privacy.nsfwDevice", s => s.Privacy?.NsfwDevice),
        ("privacy.nsfwGpuAdapter", s => s.Privacy?.NsfwGpuAdapter),
        ("ocr.enabled", s => s.Ocr?.Enabled.ToString()),
        ("ocr.engine", s => s.Ocr?.Engine),
        ("ocr.languages", s => s.Ocr?.Languages),
        ("ocr.device", s => s.Ocr?.Device),
    ];
}
