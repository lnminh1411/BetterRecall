// SPDX-License-Identifier: MIT
// Copyright (c) BetterRecall contributors
using System.Text.Json;
using System.Text.Json.Nodes;
using System.Text.RegularExpressions;

namespace BetterRecall.UI.Logic;

public enum SettingType { ReadOnlyBool, Bool, Int, Double, Choice, IntChoice, Text, Path, StringList, IntList, Hotkey, Adapter }

/// <summary>One settings key (docs/v1-design.md section 8). Label resource: "S_" + key with '.' as '_'; description: same + "_D".</summary>
public sealed record SettingDescriptor(
    string Key,
    string Group,
    SettingType Type,
    double? Min = null,
    double? Max = null,
    string[]? Choices = null,
    string? WarningKey = null,
    string? Pattern = null,
    string? EnabledWhenKey = null,
    string? EnabledWhenValue = null)
{
    /// <summary>False when another key gates this one (replay.scalePercent needs replay.resolution = scale,
    /// ipc.allowPurge needs ipc.allowControl on). Gates chain: a key is off when its gate is off.</summary>
    public bool IsEnabled(JsonObject root)
    {
        if (EnabledWhenKey is null) return true;
        var v = SettingsSchema.Get(root, EnabledWhenKey) as JsonValue;
        string? s = v is null ? null
            : v.TryGetValue<string>(out var str) ? str
            : v.TryGetValue<bool>(out var b) ? (b ? "true" : "false") : null;
        return s == EnabledWhenValue && (SettingsSchema.Find(EnabledWhenKey)?.IsEnabled(root) ?? true);
    }

    public string LabelKey => "S_" + Key.Replace('.', '_');
    public string DescriptionKey => LabelKey + "_D";
    public string ChoiceKey(string choice) => LabelKey + "_" + choice;
    public string InvalidError => "Invalid " + Key + ".";
}

public static partial class SettingsSchema
{
    /// <summary>Navigation order of the settings groups. Resource "G_" + id.</summary>
    public static readonly string[] Groups =
        ["appearance", "capture", "keyframes", "replay", "audio", "privacy", "index", "inference", "governance", "security", "ipc", "retention", "log"];   // hotkeys: own Shortcuts page

    public static readonly IReadOnlyList<SettingDescriptor> All =
    [
        new("capture.consented", "capture", SettingType.ReadOnlyBool),
        new("capture.enabled", "capture", SettingType.Bool),
        new("capture.intervalMs", "capture", SettingType.Int, 500, 60000),
        new("capture.monitors", "capture", SettingType.IntList, 0, 63),
        new("capture.maxWidth", "capture", SettingType.Int, 640, 3840),
        new("capture.dhashThreshold", "capture", SettingType.Int, 0, 32),
        new("capture.dirtyMinPercent", "capture", SettingType.Double, 0, 100),
        new("capture.eventTriggers", "capture", SettingType.Bool),
        new("capture.pauseOnFullscreen", "capture", SettingType.Bool),

        new("keyframes.enabled", "keyframes", SettingType.Bool),
        new("keyframes.format", "keyframes", SettingType.Choice, Choices: ["webp", "jpeg"]),
        new("keyframes.quality", "keyframes", SettingType.Int, 30, 95),
        new("keyframes.maxGb", "keyframes", SettingType.Int, 1, 2000),
        new("keyframes.thumbCacheMb", "keyframes", SettingType.Int, 8, 1024),

        new("replay.enabled", "replay", SettingType.Bool),
        new("replay.seconds", "replay", SettingType.Int, 15, 600),
        new("replay.fps", "replay", SettingType.IntChoice, Choices: [.. BetterRecall.Contracts.ReplaySettings.FpsChoices.Select(f => f.ToString(System.Globalization.CultureInfo.InvariantCulture))]),
        new("replay.layout", "replay", SettingType.Choice, Choices: ["composited", "perMonitor"]),
        new("replay.resolution", "replay", SettingType.Choice, Choices: ["native", "2160p", "1440p", "1080p", "900p", "720p", "540p", "scale"]),
        new("replay.scalePercent", "replay", SettingType.Int, 25, 100, EnabledWhenKey: "replay.resolution", EnabledWhenValue: "scale"),
        new("replay.encoder", "replay", SettingType.Choice, Choices: ["auto", "av1", "hevc", "h264"]),
        new("replay.encoderAdapter", "replay", SettingType.Adapter),
        new("replay.bitrateKbps", "replay", SettingType.Int, 500, 50000),
        new("replay.audio", "replay", SettingType.Bool),
        new("replay.duringFullscreen", "replay", SettingType.Bool),
        new("replay.encryptExports", "replay", SettingType.Bool, WarningKey: "W_replay_encryptExports"),
        new("replay.exportDir", "replay", SettingType.Path),
        new("replay.notify", "replay", SettingType.Bool),

        new("audio.system", "audio", SettingType.Bool),
        new("audio.mic", "audio", SettingType.Bool),
        new("audio.language", "audio", SettingType.Text, Pattern: "^(auto|[a-z]{2,3})$"),
        new("audio.maxSegmentS", "audio", SettingType.Int, 5, 30),

        new("privacy.denyProcesses", "privacy", SettingType.StringList),
        new("privacy.denyPathPrefixes", "privacy", SettingType.StringList),
        new("privacy.denyTitlePatterns", "privacy", SettingType.StringList),
        new("privacy.browserBadgeDetection", "privacy", SettingType.Bool),
        new("privacy.honorDisplayAffinity", "privacy", SettingType.Bool),
        new("privacy.pauseOnLock", "privacy", SettingType.Bool),
        new("privacy.nsfwFilter", "privacy", SettingType.Bool),
        new("privacy.nsfwThreshold", "privacy", SettingType.Double, 0.01, 0.99),
        new("privacy.nsfwGrid", "privacy", SettingType.IntChoice, Choices: ["1", "2", "3"]),
        new("privacy.nsfwDetector", "privacy", SettingType.Bool),
        new("privacy.nsfwDetectorThreshold", "privacy", SettingType.Double, 0.01, 0.99, EnabledWhenKey: "privacy.nsfwDetector", EnabledWhenValue: "true"),
        new("privacy.nsfwDevice", "privacy", SettingType.Choice, Choices: ["auto", "npu", "igpu", "gpu", "cpu"]),
        new("privacy.nsfwGpuAdapter", "privacy", SettingType.Adapter, EnabledWhenKey: "privacy.nsfwDevice", EnabledWhenValue: "gpu"),
        new("privacy.panicPurgeMinutes", "privacy", SettingType.Int, 0, 1440),
        new("privacy.recordSite", "privacy", SettingType.Bool),
        new("privacy.searchHistory", "privacy", SettingType.Bool),
        new("privacy.searchHistoryMax", "privacy", SettingType.Int, 10, 1000, EnabledWhenKey: "privacy.searchHistory", EnabledWhenValue: "true"),

        new("index.dim", "index", SettingType.IntChoice, Choices: ["768", "512", "256"], WarningKey: "W_index_dim"),
        new("index.weakMatchBelow", "index", SettingType.Double, 0, 1),

        new("inference.device", "inference", SettingType.Choice, Choices: ["auto", "npu", "gpu", "cpu"]),
        new("inference.gpuAdapter", "inference", SettingType.Adapter, EnabledWhenKey: "inference.device", EnabledWhenValue: "gpu"),
        new("inference.modelDir", "inference", SettingType.Path),
        new("inference.imageTokens", "inference", SettingType.IntChoice, Choices: ["70", "140", "280"]),
        new("inference.idleExitS", "inference", SettingType.Int, 10, 3600),
        new("inference.fake", "inference", SettingType.Bool),

        new("governance.ecoQos", "governance", SettingType.Bool),
        new("governance.deferCpuPercent", "governance", SettingType.Int, 10, 100),
        new("governance.deferGpuPercent", "governance", SettingType.Int, 10, 100),
        new("governance.pauseOnBattery", "governance", SettingType.Bool),

        new("security.excludeOwnWindows", "security", SettingType.Bool),
        new("security.requireUnlock", "security", SettingType.Bool, WarningKey: "W_security_requireUnlock"),
        new("security.unlockTimeoutMin", "security", SettingType.Int, 1, 1440),
        new("security.lockOnSessionLock", "security", SettingType.Bool),
        new("security.memoryIsolation", "security", SettingType.Bool),
        new("security.keyProtection", "security", SettingType.Choice, Choices: ["auto", "tpm", "passphrase", "dpapi"]),

        new("hotkeys.search", "hotkeys", SettingType.Hotkey),
        new("hotkeys.pause", "hotkeys", SettingType.Hotkey),
        new("hotkeys.replay", "hotkeys", SettingType.Hotkey),
        new("hotkeys.panic", "hotkeys", SettingType.Hotkey),
        new("hotkeys.timeline", "hotkeys", SettingType.Hotkey),
        new("hotkeys.home", "hotkeys", SettingType.Hotkey),
        new("hotkeys.lock", "hotkeys", SettingType.Hotkey),

        new("ipc.enabled", "ipc", SettingType.Bool, WarningKey: "W_ipc_enabled"),
        new("mcp.enabled", "ipc", SettingType.Bool, WarningKey: "W_mcp_enabled"),
        new("ipc.pipeName", "ipc", SettingType.Text, Pattern: "^[A-Za-z0-9._-]{1,64}$", EnabledWhenKey: "ipc.enabled", EnabledWhenValue: "true"),
        new("ipc.allowClients", "ipc", SettingType.StringList, EnabledWhenKey: "ipc.enabled", EnabledWhenValue: "true"),
        new("ipc.allowControl", "ipc", SettingType.Bool, EnabledWhenKey: "ipc.enabled", EnabledWhenValue: "true"),
        new("ipc.allowPurge", "ipc", SettingType.Bool, EnabledWhenKey: "ipc.allowControl", EnabledWhenValue: "true"),
        new("ipc.maxPauseS", "ipc", SettingType.Int, 60, 604800, EnabledWhenKey: "ipc.allowControl", EnabledWhenValue: "true"),
        new("ipc.allowThumbnails", "ipc", SettingType.Bool),
        new("ipc.requireUnlocked", "ipc", SettingType.Bool),
        new("ipc.maxResults", "ipc", SettingType.Int, 1, 200),
        new("ipc.defaultResults", "ipc", SettingType.Int, 1, 200),
        new("ipc.maxResponseBytes", "ipc", SettingType.Int, 4096, 1048576),
        new("ipc.maxSearchPerMinute", "ipc", SettingType.Int, 1, 600),
        new("ipc.idleTimeoutS", "ipc", SettingType.Int, 10, 3600),

        new("retention.days", "retention", SettingType.Int, 1, 3650),

        new("log.level", "log", SettingType.Choice, Choices: ["debug", "info", "warn", "error"]),

        new("ui.theme", "appearance", SettingType.Choice, Choices: ["system", "light", "dark"]),
        new("ui.searchAlwaysOnTop", "appearance", SettingType.Bool),
        new("ui.startMinimized", "appearance", SettingType.Bool),
    ];

    public static SettingDescriptor? Find(string key) => All.FirstOrDefault(d => string.Equals(d.Key, key, StringComparison.OrdinalIgnoreCase));

    public static IEnumerable<SettingDescriptor> InGroup(string group) => All.Where(d => d.Group == group);

    /// <summary>Names offered by an Adapter setting's list: every GPU of the last benchmark (integrated ones
    /// included) except the Microsoft Basic Render Driver (vendor 0x1414, a software adapter), each name once.</summary>
    public static IReadOnlyList<string> AdapterChoices(IEnumerable<Contracts.GpuInfo>? gpus)
        => (gpus ?? []).Where(g => !string.IsNullOrWhiteSpace(g.Name)
                                   && !string.Equals(g.Vendor, "0x1414", StringComparison.OrdinalIgnoreCase)
                                   && !g.Name.Contains("Basic Render", StringComparison.OrdinalIgnoreCase))
                       .Select(g => g.Name).Distinct(StringComparer.Ordinal).ToList();

    // ---- path access on a settings JsonObject ----

    public static JsonNode? Get(JsonObject root, string key)
    {
        JsonNode? node = root;
        foreach (var part in key.Split('.'))
        {
            if (node is not JsonObject o || !o.TryGetPropertyValue(part, out node)) return null;
        }
        return node;
    }

    public static void Set(JsonObject root, string key, JsonNode? value)
    {
        var parts = key.Split('.');
        var obj = root;
        for (int i = 0; i < parts.Length - 1; i++)
        {
            if (obj[parts[i]] is not JsonObject child)
            {
                child = new JsonObject();
                obj[parts[i]] = child;
            }
            obj = child;
        }
        obj[parts[^1]] = value;
    }

    // ---- validation (mirrors Core so errors show before a round trip) ----

    /// <summary>Null when valid, else "Invalid key.".</summary>
    public static string? Validate(SettingDescriptor d, JsonObject root)
    {
        var v = Get(root, d.Key);
        bool ok = d.Type switch
        {
            SettingType.ReadOnlyBool or SettingType.Bool => v is JsonValue bv && bv.TryGetValue<bool>(out _),
            SettingType.Int => TryNumber(v, out var n) && n == Math.Floor(n) && InRange(d, n) && ExtraIntRule(d, root, n),
            SettingType.Double => TryNumber(v, out var x) && !double.IsNaN(x) && InRange(d, x),
            SettingType.Choice => v is JsonValue cv && cv.TryGetValue<string>(out var s) && d.Choices!.Contains(s),
            SettingType.IntChoice => TryNumber(v, out var ic) && d.Choices!.Contains(((long)ic).ToString(System.Globalization.CultureInfo.InvariantCulture)),
            SettingType.Adapter => v is JsonValue av && av.TryGetValue<string>(out var a) && a.Trim().Length is > 0 and <= 256,
            SettingType.Text => v is JsonValue tv && tv.TryGetValue<string>(out var t) && (d.Pattern is null || Regex.IsMatch(t, d.Pattern)),
            SettingType.Path => v is JsonValue pv && pv.TryGetValue<string>(out var p) && (p.Length == 0 || IsAbsolutePath(p)),
            SettingType.StringList => v is JsonArray a && a.All(e => e is JsonValue ev && ev.TryGetValue<string>(out var es) && es.Trim().Length > 0)
                                      && (d.Key != "ipc.allowClients" || a.All(e => IsAbsolutePath(e!.GetValue<string>()))),
            SettingType.IntList => v is JsonArray ia && ia.All(e => TryNumber(e, out var en) && en == Math.Floor(en) && InRange(d, en)),
            SettingType.Hotkey => v is JsonValue hv && hv.TryGetValue<string>(out var h) && (h.Length == 0 || (HotkeyFormat.Normalize(h) == h && !HotkeyClash(d, root, h))),   // "" = not assigned
            _ => true,
        };
        return ok ? null : d.InvalidError;
    }

    /// <summary>All errors keyed by setting key.</summary>
    public static Dictionary<string, string> ValidateAll(JsonObject root)
    {
        var errors = new Dictionary<string, string>();
        foreach (var d in All)
            if (Validate(d, root) is { } e) errors[d.Key] = e;
        return errors;
    }

    private static bool ExtraIntRule(SettingDescriptor d, JsonObject root, double n) => d.Key switch
    {
        "ipc.defaultResults" => !TryNumber(Get(root, "ipc.maxResults"), out var max) || n <= max,
        _ => true,
    };

    private static bool HotkeyClash(SettingDescriptor d, JsonObject root, string value) =>
        All.Where(o => o.Type == SettingType.Hotkey && o.Key != d.Key)
           .Any(o => Get(root, o.Key) is JsonValue ov && ov.TryGetValue<string>(out var s) && string.Equals(s, value, StringComparison.OrdinalIgnoreCase));

    private static bool InRange(SettingDescriptor d, double n) => (d.Min is not { } lo || n >= lo) && (d.Max is not { } hi || n <= hi);

    public static bool TryNumber(JsonNode? v, out double n)
    {
        n = 0;
        if (v is not JsonValue jv) return false;
        if (jv.TryGetValue<double>(out n)) return true;
        if (jv.TryGetValue<int>(out var i)) { n = i; return true; }
        if (jv.TryGetValue<long>(out var l)) { n = l; return true; }
        if (jv.TryGetValue<JsonElement>(out var e) && e.ValueKind == JsonValueKind.Number) { n = e.GetDouble(); return true; }
        return false;
    }

    public static bool IsAbsolutePath(string p)
    {
        try { return Path.IsPathFullyQualified(p) && p.IndexOfAny(Path.GetInvalidPathChars()) < 0; }
        catch { return false; }
    }
}
