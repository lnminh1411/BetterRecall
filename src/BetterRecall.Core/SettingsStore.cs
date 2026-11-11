// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Text.Json;
using System.Text.Json.Nodes;
using System.Text.RegularExpressions;
using BetterRecall.Contracts;

namespace BetterRecall.Core;

/// <summary>settings.json with validation (v1-design section 8, research 7.2). Thread-safe.</summary>
public sealed partial class SettingsStore : ISettingsStore
{
    private readonly string _path;
    private readonly Lock _gate = new();
    private Settings _current;

    public event Action<Settings>? Changed;

    private SettingsStore(string path, Settings current)
    {
        _path = path;
        _current = current;
    }

    /// <summary>Current settings snapshot. Treat it as read-only: every update replaces the instance.</summary>
    public Settings Current { get { lock (_gate) return _current; } }

    public string FilePath => _path;

    /// <summary>Loads settings.json from home. Missing file: defaults, written out. Bad keys are reset to defaults and logged.</summary>
    public static SettingsStore Load(string home)
    {
        var path = DataPaths.SettingsFile(home);
        Settings s = new();
        bool write = false;
        if (File.Exists(path))
        {
            try
            {
                var json = File.ReadAllText(path);
                var node = JsonNode.Parse(json, documentOptions: new JsonDocumentOptions { CommentHandling = JsonCommentHandling.Skip, AllowTrailingCommas = true });
                if (node is not JsonObject obj) throw new JsonException("Not an object.");
                s = LoadLenient(obj, out write);
            }
            catch (Exception ex)
            {
                Log.Warn("settings.json unreadable, defaults used; bad file kept as settings.json.bad", ex);
                try { File.Copy(path, path + ".bad", overwrite: true); } catch { }
                s = new Settings();
                write = true;
            }
        }
        else write = true;

        var store = new SettingsStore(path, s);
        if (write) { try { store.Save(); } catch (Exception ex) { Log.Warn("settings.json could not be written", ex); } }
        return store;
    }

    /// <summary>Reads a settings file without creating or fixing it (CLI, MCP). Defaults when missing or bad.</summary>
    public static Settings ReadOnly(string home)
    {
        try
        {
            var path = DataPaths.SettingsFile(home);
            if (!File.Exists(path)) return new Settings();
            var node = JsonNode.Parse(File.ReadAllText(path), documentOptions: new JsonDocumentOptions { CommentHandling = JsonCommentHandling.Skip, AllowTrailingCommas = true });
            return node is JsonObject obj ? LoadLenient(obj, out _) : new Settings();
        }
        catch { return new Settings(); }
    }

    /// <summary>Applies each known key of obj over the defaults, skipping keys that do not parse or validate.</summary>
    private static Settings LoadLenient(JsonObject obj, out bool changed)
    {
        changed = MigrateLegacy(obj);
        var schema = (JsonObject)JsonSerializer.SerializeToNode(new Settings(), ContractsJson.Default.Settings)!;
        var merged = (JsonObject)schema.DeepClone();
        foreach (var (section, sectionNode) in obj)
        {
            if (schema[section] is not JsonObject || sectionNode is not JsonObject sec) { changed = true; continue; }
            foreach (var (key, value) in sec)
            {
                if (((JsonObject)schema[section]!)[key] is null && !((JsonObject)schema[section]!).ContainsKey(key)) { changed = true; continue; }
                var candidate = (JsonObject)merged.DeepClone();
                ((JsonObject)candidate[section]!)[key] = value?.DeepClone();
                var parsed = TryDeserialize(candidate);
                if (parsed == null || Validate(parsed) is { } bad && bad == $"{section}.{key}")
                {
                    Log.Warn($"settings: {section}.{key} invalid, default kept");
                    changed = true;
                    continue;
                }
                merged = candidate;
            }
        }
        var result = TryDeserialize(merged) ?? new Settings();
        // Cross-key rules can still fail (defaultResults above maxResults): reset the offending key.
        for (int i = 0; i < 64 && Validate(result) is { } key; i++)
        {
            Log.Warn($"settings: {key} invalid, reset to default");
            var parts = key.Split('.');
            ((JsonObject)merged[parts[0]]!)[parts[1]] = schema[parts[0]]![parts[1]]?.DeepClone();
            result = TryDeserialize(merged) ?? new Settings();
            changed = true;
        }
        return result;
    }

    /// <summary>
    /// Files written before 0.2 batch 2 carry keyframes.maxGb: it moves to storage.maxGb, and the old defaults
    /// capture.dirtyMinPercent 0.5 and keyframes.format webp move to the new ones (15, avif). Values the user
    /// changed stay. True when the file needs rewriting.
    /// </summary>
    internal static bool MigrateLegacy(JsonObject obj)
    {
        if (obj["keyframes"] is not JsonObject kf || !kf.ContainsKey("maxGb")) return false;
        var old = kf["maxGb"]?.DeepClone();
        kf.Remove("maxGb");
        if (obj["storage"] is not JsonObject st) obj["storage"] = st = new JsonObject();
        if (!st.ContainsKey("maxGb") && old is not null) st["maxGb"] = old;
        if (kf["format"] is JsonValue f && f.TryGetValue<string>(out var fs) && fs == "webp") kf["format"] = "avif";
        if (obj["capture"] is JsonObject cap && cap["dirtyMinPercent"] is JsonValue d && d.TryGetValue<double>(out var dv) && dv == 0.5)
            cap["dirtyMinPercent"] = 15.0;
        Log.Info("settings: keyframes.maxGb moved to storage.maxGb");
        return true;
    }

    private static Settings? TryDeserialize(JsonObject node)
    {
        try { return node.Deserialize(ContractsJson.Default.Settings); }
        catch (JsonException) { return null; }
        catch (InvalidOperationException) { return null; }
    }

    public string? TryUpdate(JsonElement partial)
    {
        if (partial.ValueKind != JsonValueKind.Object) return "Invalid JSON.";
        Settings next;
        lock (_gate)
        {
            var schema = (JsonObject)JsonSerializer.SerializeToNode(new Settings(), ContractsJson.Default.Settings)!;
            var merged = (JsonObject)JsonSerializer.SerializeToNode(_current, ContractsJson.Default.Settings)!;
            var touched = new List<string>();
            foreach (var section in partial.EnumerateObject())
            {
                if (schema[section.Name] is not JsonObject schemaSection) return $"Invalid {section.Name}.";
                if (section.Value.ValueKind != JsonValueKind.Object) return $"Invalid {section.Name}.";
                foreach (var key in section.Value.EnumerateObject())
                {
                    if (!schemaSection.ContainsKey(key.Name)) return $"Invalid {section.Name}.{key.Name}.";
                    if (!SameKind(schemaSection[key.Name], key.Value)) return $"Invalid {section.Name}.{key.Name}.";
                    var candidate = (JsonObject)merged.DeepClone();
                    ((JsonObject)candidate[section.Name]!)[key.Name] = JsonNode.Parse(key.Value.GetRawText());
                    if (TryDeserialize(candidate) == null) return $"Invalid {section.Name}.{key.Name}.";
                    merged = candidate;
                    touched.Add($"{section.Name}.{key.Name}");
                }
            }
            var parsed = TryDeserialize(merged);
            if (parsed == null) return "Invalid JSON.";
            var bad = Validate(parsed);
            if (bad != null)
            {
                // Attribute a cross-key failure to the key the caller changed.
                if (bad == "ipc.defaultResults" && !touched.Contains(bad) && touched.Contains("ipc.maxResults")) bad = "ipc.maxResults";
                return $"Invalid {bad}.";
            }
            next = parsed;
            WriteFile(next);
            _current = next;
        }
        RaiseChanged(next);
        return null;
    }

    private static bool SameKind(JsonNode? schemaValue, JsonElement value)
    {
        if (schemaValue == null) return true;
        var expected = schemaValue.GetValueKind();
        return expected switch
        {
            JsonValueKind.True or JsonValueKind.False => value.ValueKind is JsonValueKind.True or JsonValueKind.False,
            _ => value.ValueKind == expected,
        };
    }

    public void Save()
    {
        Settings snapshot;
        lock (_gate) { snapshot = _current; WriteFile(snapshot); }
    }

    /// <summary>Replaces the settings in code (agent use, for example capture.consented). Validates first.</summary>
    public string? Replace(Settings next)
    {
        var bad = Validate(next);
        if (bad != null) return $"Invalid {bad}.";
        lock (_gate) { WriteFile(next); _current = next; }
        RaiseChanged(next);
        return null;
    }

    private void RaiseChanged(Settings s)
    {
        var handlers = Changed;
        if (handlers == null) return;
        foreach (var d in handlers.GetInvocationList())
        {
            try { ((Action<Settings>)d)(s); }
            catch (Exception ex) { Log.Error("settings Changed handler failed", ex); }
        }
    }

    private void WriteFile(Settings s)
    {
        Directory.CreateDirectory(Path.GetDirectoryName(_path)!);
        var json = JsonSerializer.Serialize(s, PrettySettingsJson.Default.Settings);
        var tmp = _path + ".tmp";
        using (var fs = new FileStream(tmp, FileMode.Create, FileAccess.Write, FileShare.None))
        {
            var bytes = System.Text.Encoding.UTF8.GetBytes(json);
            fs.Write(bytes);
            fs.Flush(flushToDisk: true);
        }
        File.Move(tmp, _path, overwrite: true);
    }

    // ---- validation ----

    private static readonly string[] Modifiers = ["Ctrl", "Alt", "Shift", "Win"];
    // Hotkey grammar of v1-design section 5 (agent parser and UI recorder use the same list).
    private static readonly HashSet<string> NamedKeys = new(StringComparer.OrdinalIgnoreCase)
    {
        "Space", "PageUp", "PageDown", "Home", "End", "Left", "Up", "Right", "Down", "Insert", "Delete", "Pause", "PrintScreen",
        "NumPad0", "NumPad1", "NumPad2", "NumPad3", "NumPad4", "NumPad5", "NumPad6", "NumPad7", "NumPad8", "NumPad9",
        ";", "=", ",", "-", ".", "/", "`", "[", "\\", "]", "'",
    };

    [GeneratedRegex("^[A-Za-z0-9._-]{1,64}$")]
    private static partial Regex PipeNameRegex();

    [GeneratedRegex("^[a-z]{2,3}$")]
    private static partial Regex LanguageRegex();

    internal static bool ValidHotkey(string? s)
    {
        if (string.IsNullOrWhiteSpace(s)) return false;
        var parts = s.Split('+', StringSplitOptions.TrimEntries);
        if (parts.Length < 2) return false;
        var mods = parts[..^1];
        if (mods.Any(m => !Modifiers.Contains(m, StringComparer.OrdinalIgnoreCase))) return false;
        if (mods.Distinct(StringComparer.OrdinalIgnoreCase).Count() != mods.Length) return false;
        var key = parts[^1];
        if (key.Length == 1 && char.IsAsciiLetterOrDigit(key[0])) return true;
        if (key.Length is >= 2 and <= 3 && (key[0] == 'F' || key[0] == 'f') && int.TryParse(key[1..], out var f) && f is >= 1 and <= 24) return true;
        return NamedKeys.Contains(key);
    }

    private static bool AbsOrEmpty(string? p) => p != null && (p.Length == 0 || IsAbs(p));
    private static bool IsAbs(string? p) => !string.IsNullOrWhiteSpace(p) && p.IndexOfAny(Path.GetInvalidPathChars()) < 0 && Path.IsPathFullyQualified(p);
    private static bool In(string? v, params string[] allowed) => v != null && allowed.Contains(v);
    private static bool Range(int v, int lo, int hi) => v >= lo && v <= hi;
    private static bool Range(double v, double lo, double hi) => !double.IsNaN(v) && v >= lo && v <= hi;
    private static bool Strings(string[]? a, Func<string, bool> each) => a != null && a.Length <= 1024 && a.All(x => x != null && each(x));

    private static readonly (string Key, Func<Settings, bool> Ok)[] Rules =
    [
        ("capture.intervalMs", s => Range(s.Capture.IntervalMs, 500, 60000)),
        ("capture.monitors", s => s.Capture.Monitors != null && s.Capture.Monitors.Length <= 64 && s.Capture.Monitors.All(m => m is >= 0 and < 64)),
        ("capture.maxWidth", s => Range(s.Capture.MaxWidth, 640, 3840)),
        ("capture.dhashThreshold", s => Range(s.Capture.DhashThreshold, 0, 32)),
        ("capture.dirtyMinPercent", s => Range(s.Capture.DirtyMinPercent, 0, 100)),
        ("keyframes.format", s => In(s.Keyframes.Format, "avif", "webp", "jxl", "jpeg")),
        ("keyframes.quality", s => Range(s.Keyframes.Quality, 30, 95)),
        ("keyframes.encoder", s => In(s.Keyframes.Encoder, "auto", "hardware", "software")),
        ("keyframes.thumbCacheMb", s => Range(s.Keyframes.ThumbCacheMb, 8, 1024)),
        ("replay.seconds", s => Range(s.Replay.Seconds, 15, 600)),
        ("replay.fps", s => ReplaySettings.FpsChoices.Contains(s.Replay.Fps)),
        ("replay.bitrateKbps", s => Range(s.Replay.BitrateKbps, 500, 50000)),
        ("replay.encoder", s => In(s.Replay.Encoder, "auto", "av1", "hevc", "h264")),
        ("replay.encoderAdapter", s => s.Replay.EncoderAdapter is { Length: > 0 and <= 128 } a && !string.IsNullOrWhiteSpace(a)),
        ("replay.layout", s => In(s.Replay.Layout, "composited", "perMonitor")),
        ("replay.resolution", s => In(s.Replay.Resolution, "native", "2160p", "1440p", "1080p", "900p", "720p", "540p", "scale")),
        ("replay.scalePercent", s => Range(s.Replay.ScalePercent, 25, 100)),
        ("replay.exportDir", s => AbsOrEmpty(s.Replay.ExportDir)),
        ("index.dim", s => s.Index.Dim is 768 or 512 or 256),
        ("index.weakMatchBelow", s => Range(s.Index.WeakMatchBelow, 0.0, 1.0)),
        ("retention.days", s => Range(s.Retention.Days, 1, 3650)),
        ("audio.language", s => s.Audio.Language == "auto" || (s.Audio.Language != null && LanguageRegex().IsMatch(s.Audio.Language))),
        ("audio.maxSegmentS", s => Range(s.Audio.MaxSegmentS, 5, 30)),
        ("privacy.denyProcesses", s => Strings(s.Privacy.DenyProcesses, x => x.Trim().Length is > 0 and <= 260)),
        ("privacy.denyPathPrefixes", s => Strings(s.Privacy.DenyPathPrefixes, IsAbs)),
        ("privacy.denyTitlePatterns", s => Strings(s.Privacy.DenyTitlePatterns, x => x.Length is > 0 and <= 256)),
        ("privacy.nsfwThreshold", s => Range(s.Privacy.NsfwThreshold, 0.01, 0.99)),
        ("privacy.nsfwGrid", s => Range(s.Privacy.NsfwGrid, 1, 3)),
        ("privacy.nsfwDetectorThreshold", s => Range(s.Privacy.NsfwDetectorThreshold, 0.01, 0.99)),
        ("privacy.nsfwDevice", s => In(s.Privacy.NsfwDevice, "auto", "npu", "igpu", "gpu", "cpu")),
        ("privacy.nsfwGpuAdapter", s => s.Privacy.NsfwGpuAdapter is { Length: > 0 and <= 128 } a && !string.IsNullOrWhiteSpace(a)),
        ("privacy.panicPurgeMinutes", s => Range(s.Privacy.PanicPurgeMinutes, 0, 1440)),
        ("privacy.searchHistoryMax", s => Range(s.Privacy.SearchHistoryMax, 10, 1000)),
        ("inference.device", s => In(s.Inference.Device, "auto", "npu", "gpu", "cpu")),
        ("inference.gpuAdapter", s => s.Inference.GpuAdapter is { Length: > 0 and <= 128 } a && !string.IsNullOrWhiteSpace(a)),
        ("inference.modelDir", s => AbsOrEmpty(s.Inference.ModelDir)),
        ("inference.imageTokens", s => s.Inference.ImageTokens is 70 or 140 or 280),
        ("inference.idleExitS", s => Range(s.Inference.IdleExitS, 10, 3600)),
        ("governance.deferCpuPercent", s => Range(s.Governance.DeferCpuPercent, 10, 100)),
        ("governance.deferGpuPercent", s => Range(s.Governance.DeferGpuPercent, 10, 100)),
        ("security.unlockTimeoutMin", s => Range(s.Security.UnlockTimeoutMin, 1, 1440)),
        ("security.keyProtection", s => In(s.Security.KeyProtection, "auto", "tpm", "passphrase", "dpapi")),
        ("hotkeys.search", s => ValidHotkey(s.Hotkeys.Search)),
        ("hotkeys.pause", s => ValidHotkey(s.Hotkeys.Pause)),
        ("hotkeys.replay", s => ValidHotkey(s.Hotkeys.Replay)),
        ("hotkeys.panic", s => ValidHotkey(s.Hotkeys.Panic)),
        ("ipc.pipeName", s => s.Ipc.PipeName != null && PipeNameRegex().IsMatch(PipeNames.Base(s.Ipc.PipeName))),
        ("ipc.allowClients", s => Strings(s.Ipc.AllowClients, IsAbs)),
        ("ipc.maxResults", s => Range(s.Ipc.MaxResults, 1, 200)),
        ("ipc.defaultResults", s => Range(s.Ipc.DefaultResults, 1, s.Ipc.MaxResults)),
        ("ipc.maxResponseBytes", s => Range(s.Ipc.MaxResponseBytes, 4096, 1048576)),
        ("ipc.maxSearchPerMinute", s => Range(s.Ipc.MaxSearchPerMinute, 1, 600)),
        ("ipc.maxPauseS", s => Range(s.Ipc.MaxPauseS, 60, 604800)),
        ("ipc.idleTimeoutS", s => Range(s.Ipc.IdleTimeoutS, 10, 3600)),
        ("log.level", s => In(s.Log.Level, "debug", "info", "warn", "error")),
        ("ui.theme", s => In(s.Ui.Theme, "system", "light", "dark")),
        ("retention.progressiveAfterDays", s => Range(s.Retention.ProgressiveAfterDays, 0, 3650)),
        ("retention.progressiveOverDays", s => Range(s.Retention.ProgressiveOverDays, 1, 3650)),
        ("storage.minFreeGb", s => Range(s.Storage.MinFreeGb, 0, 10000)),
        ("storage.maxGb", s => Range(s.Storage.MaxGb, 1, 20000)),
    ];

    /// <summary>First invalid key as "section.key", or null when every key is valid.</summary>
    public static string? Validate(Settings s)
    {
        if (s.Capture == null) return "capture";
        if (s.Keyframes == null) return "keyframes";
        if (s.Replay == null) return "replay";
        if (s.Index == null) return "index";
        if (s.Retention == null) return "retention";
        if (s.Audio == null) return "audio";
        if (s.Privacy == null) return "privacy";
        if (s.Inference == null) return "inference";
        if (s.Governance == null) return "governance";
        if (s.Security == null) return "security";
        if (s.Hotkeys == null) return "hotkeys";
        if (s.Ipc == null) return "ipc";
        if (s.Mcp == null) return "mcp";
        if (s.Log == null) return "log";
        if (s.Ui == null) return "ui";
        if (s.Storage == null) return "storage";
        foreach (var (key, ok) in Rules)
        {
            bool pass;
            try { pass = ok(s); } catch { pass = false; }
            if (!pass) return key;
        }
        return null;
    }
}

[System.Text.Json.Serialization.JsonSourceGenerationOptions(
    PropertyNamingPolicy = System.Text.Json.Serialization.JsonKnownNamingPolicy.CamelCase,
    WriteIndented = true)]
[System.Text.Json.Serialization.JsonSerializable(typeof(Settings))]
internal partial class PrettySettingsJson : System.Text.Json.Serialization.JsonSerializerContext;
