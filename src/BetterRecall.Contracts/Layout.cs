// SPDX-License-Identifier: MIT
// Copyright (c) BetterRecall contributors
using System.Security.Principal;
using System.Text.Json.Serialization;

namespace BetterRecall.Contracts;

/// <summary>Exe paths relative to the install dir. Trust checks compare full image paths against these.</summary>
public static class InstallLayout
{
    public const string AgentExe = "BetterRecall.Agent.exe";
    public const string CliExe = "betterrecall.exe";
    public const string McpExe = "betterrecall-mcp.exe";
    public const string WorkerExe = @"worker\BetterRecall.Worker.exe";
    public const string UiExe = @"ui\BetterRecall.UI.exe";

    /// <summary>Set by the single-file launcher for the agent it starts: full path of the launcher exe.
    /// Autostart then points at the launcher, which survives version upgrades.</summary>
    public const string LauncherEnv = "BETTERRECALL_LAUNCHER";

    public static string Full(string installDir, string relative) => Path.GetFullPath(Path.Combine(installDir, relative));
}

/// <summary>Data dir layout. BETTERRECALL_HOME overrides %LOCALAPPDATA%\BetterRecall.</summary>
public static class DataPaths
{
    public const string HomeEnv = "BETTERRECALL_HOME";

    public static string Home()
    {
        var env = Environment.GetEnvironmentVariable(HomeEnv);
        if (!string.IsNullOrWhiteSpace(env)) return Path.GetFullPath(env);
        return Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.LocalApplicationData), "BetterRecall");
    }

    public static string SettingsFile(string home) => Path.Combine(home, "settings.json");
    public static string StoreDb(string home) => Path.Combine(home, "store.db");
    public static string KeyframesDir(string home) => Path.Combine(home, "keyframes");
    public static string ReplaysDir(string home) => Path.Combine(home, "replays");
    public static string KeyFile(string home) => Path.Combine(home, "keys", "datakey.bin");
    /// <summary>Argon2id hash of the unlock password (security.unlockMethod password) plus the retry counter.</summary>
    public static string UnlockPasswordFile(string home) => Path.Combine(home, "keys", "unlock.bin");
    /// <summary>Cloud describe API key, DPAPI-encrypted (CurrentUser).</summary>
    public static string AiKeyFile(string home) => Path.Combine(home, "keys", "ai-key.bin");
    public static string ModelsDir(string home, Settings s) =>
        string.IsNullOrWhiteSpace(s.Inference.ModelDir) ? Path.Combine(home, "models") : s.Inference.ModelDir;
    public static string CompiledCache(string home) => Path.Combine(home, "cache", "compiled");
    public static string DeviceCache(string home) => Path.Combine(home, "cache", "device.json");
    public static string BenchReport(string home) => Path.Combine(home, "cache", "bench.json");
    public static string LogsDir(string home) => Path.Combine(home, "logs");
}

public static class PipeNames
{
    public static string UserSid() => WindowsIdentity.GetCurrent().User!.Value;

    /// <summary>Strips \\.\pipe\ or \pipe\ prefixes.</summary>
    public static string Base(string configured)
    {
        foreach (var prefix in new[] { @"\\.\pipe\", @"\pipe\" })
            if (configured.StartsWith(prefix, StringComparison.OrdinalIgnoreCase)) return configured[prefix.Length..];
        return configured;
    }

    public static string Public(string configuredBase, string sid) => $"{Base(configuredBase)}.{sid}";
    public static string Mcp(string sid) => $"BetterRecall.mcp.{sid}";
    public static string Ui(string sid) => $"BetterRecall.ui.{sid}";
    public static string NewWorker() => $"BetterRecall.worker.{Convert.ToHexString(System.Security.Cryptography.RandomNumberGenerator.GetBytes(16)).ToLowerInvariant()}";

    public const string AgentMutex = @"Local\BetterRecall.Agent";

    /// <summary>Auto-reset event the agent sets to show the launcher of an already running UI.</summary>
    public const string ShowLauncherEvent = @"Local\BetterRecall.UI.ShowLauncher";

    /// <summary>Auto-reset event that asks the running agent to quit, as the tray Quit does. The single-file launcher
    /// sets it when a different version is already running, so the new one can take over.</summary>
    public const string AgentQuitEvent = @"Local\BetterRecall.Agent.Quit";
}

/// <summary>Writes DateTimeOffset as ISO 8601 UTC with Z (research.md 7.5). Reads any ISO 8601 offset.</summary>
public sealed class UtcTimestampConverter : JsonConverter<DateTimeOffset>
{
    public override DateTimeOffset Read(ref System.Text.Json.Utf8JsonReader reader, Type typeToConvert, System.Text.Json.JsonSerializerOptions options)
    {
        var s = reader.GetString();
        if (s != null && DateTimeOffset.TryParse(s, System.Globalization.CultureInfo.InvariantCulture, System.Globalization.DateTimeStyles.AssumeUniversal, out var v))
            return v.ToUniversalTime();
        throw new System.Text.Json.JsonException("Bad timestamp.");
    }

    public override void Write(System.Text.Json.Utf8JsonWriter writer, DateTimeOffset value, System.Text.Json.JsonSerializerOptions options)
        => writer.WriteStringValue(value.UtcDateTime.ToString("yyyy-MM-dd'T'HH:mm:ss.FFF'Z'", System.Globalization.CultureInfo.InvariantCulture));
}

/// <summary>Source-generated JSON for everything in Contracts. camelCase, nulls omitted.</summary>
[JsonSourceGenerationOptions(
    PropertyNamingPolicy = JsonKnownNamingPolicy.CamelCase,
    DefaultIgnoreCondition = JsonIgnoreCondition.WhenWritingNull,
    WriteIndented = false,
    Converters = [typeof(UtcTimestampConverter)])]
[JsonSerializable(typeof(Settings))]
[JsonSerializable(typeof(CaptureMeta))]
[JsonSerializable(typeof(List<CaptureMeta>))]
[JsonSerializable(typeof(SearchRequest))]
[JsonSerializable(typeof(SearchResponse))]
[JsonSerializable(typeof(SearchHit))]
[JsonSerializable(typeof(CaptureDetail))]
[JsonSerializable(typeof(ThumbData))]
[JsonSerializable(typeof(StoreStats))]
[JsonSerializable(typeof(AgentStatus))]
[JsonSerializable(typeof(AuditEntry))]
[JsonSerializable(typeof(List<AuditEntry>))]
[JsonSerializable(typeof(PurgeResult))]
[JsonSerializable(typeof(ReplayClip))]
[JsonSerializable(typeof(ItemsReply<CaptureMeta>))]
[JsonSerializable(typeof(ItemsReply<AuditEntry>))]
[JsonSerializable(typeof(ItemsReply<ReplayClip>))]
[JsonSerializable(typeof(UnlockReply))]
[JsonSerializable(typeof(PurgeConfirmReply))]
[JsonSerializable(typeof(FacetsReply))]
[JsonSerializable(typeof(ItemsReply<HistoryEntry>))]
[JsonSerializable(typeof(VersionReply))]
[JsonSerializable(typeof(List<ReplayClip>))]
[JsonSerializable(typeof(PasswordStatus))]
[JsonSerializable(typeof(AiStatus))]
[JsonSerializable(typeof(DescribePreview))]
[JsonSerializable(typeof(DescribeResult))]
[JsonSerializable(typeof(WorkerHello))]
[JsonSerializable(typeof(TranscribeResult))]
[JsonSerializable(typeof(BenchResult))]
[JsonSerializable(typeof(BenchReport))]
[JsonSerializable(typeof(WorkerRequest))]
[JsonSerializable(typeof(WorkerResponse))]
[JsonSerializable(typeof(string))]
public partial class ContractsJson : JsonSerializerContext;
