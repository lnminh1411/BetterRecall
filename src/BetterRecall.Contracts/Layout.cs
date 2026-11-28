// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Runtime.InteropServices;
using System.Security.Principal;
using System.Text;
using System.Text.Json.Serialization;

namespace BetterRecall.Contracts;

/// <summary>Exe paths relative to the install dir. Trust checks compare full image paths against these.</summary>
public static partial class InstallLayout
{
    public const string AgentExe = "BetterRecall.Agent.exe";
    public const string CliExe = "betterrecall.exe";
    public const string McpExe = "betterrecall-mcp.exe";
    public const string WorkerExe = @"worker\BetterRecall.Worker.exe";
    public const string UiExe = @"ui\BetterRecall.UI.exe";

    public static string Full(string installDir, string relative) => Path.GetFullPath(Path.Combine(installDir, relative));

    public const string CliShim = "betterrecall.cmd";

    /// <summary>Set by the bin\betterrecall-mcp.exe relay for the MCP server it starts: the relay restarts a server that
    /// exits with McpRestartExitCode against the current install and sends it the requests left unanswered.</summary>
    public const string McpRelayEnv = "BETTERRECALL_MCP_RELAY";
    public const int McpRestartExitCode = 75;

    /// <summary>A launcher install folder name: "&lt;version&gt;-&lt;hash12&gt;" (12 lower-case hex digits).</summary>
    public static bool IsVersionFolder(string name)
    {
        int dash = name.LastIndexOf('-');
        if (dash <= 0 || name.Length - dash - 1 != 12 || !char.IsAsciiDigit(name[0])) return false;
        for (int i = dash + 1; i < name.Length; i++)
            if (!char.IsAsciiHexDigitLower(name[i]) && !char.IsAsciiDigit(name[i])) return false;
        return true;
    }

    /// <summary>The path with SUBST drives, junctions and symbolic links resolved (GetFinalPathNameByHandle); for a
    /// path not created yet, its nearest existing ancestor resolved plus the rest. The input when nothing resolves.</summary>
    public static string RealPath(string path)
    {
        if (string.IsNullOrWhiteSpace(path)) return path;
        string full;
        try { full = Path.GetFullPath(path); }
        catch (Exception e) when (e is ArgumentException or NotSupportedException or PathTooLongException) { return path; }
        if (full.Length > 3) full = full.TrimEnd('\\');
        var rest = new Stack<string>();
        for (string? cur = full; cur is not null; cur = Path.GetDirectoryName(cur))
        {
            if ((Directory.Exists(cur) || File.Exists(cur)) && FinalPath(cur) is { } real)
            {
                foreach (var part in rest) real = Path.Combine(real, part);
                return real;
            }
            var leaf = Path.GetFileName(cur);
            if (leaf.Length > 0) rest.Push(leaf);
            else break;
        }
        return full;
    }

    private static string? FinalPath(string path)
    {
        nint h = CreateFileW(path, 0, 7 /* share read, write, delete */, 0, 3 /* OPEN_EXISTING */, 0x02000000 /* FILE_FLAG_BACKUP_SEMANTICS */, 0);
        if (h == -1) return null;
        try
        {
            var buf = new char[1024];
            uint n = GetFinalPathNameByHandleW(h, buf, (uint)buf.Length, 0);
            if (n == 0) return null;
            if (n >= buf.Length)
            {
                buf = new char[n + 1];
                n = GetFinalPathNameByHandleW(h, buf, (uint)buf.Length, 0);
                if (n == 0 || n >= buf.Length) return null;
            }
            var s = new string(buf, 0, (int)n);
            if (s.StartsWith(@"\\?\UNC\", StringComparison.OrdinalIgnoreCase)) return @"\\" + s[8..];
            if (s.StartsWith(@"\\?\", StringComparison.Ordinal)) s = s[4..];
            return s.Length > 3 ? s.TrimEnd('\\') : s;
        }
        finally { CloseHandle(h); }
    }

    [LibraryImport("kernel32.dll", StringMarshalling = StringMarshalling.Utf16, SetLastError = true)]
    private static partial nint CreateFileW(string name, uint access, uint share, nint security, uint disposition, uint flags, nint template);

    [LibraryImport("kernel32.dll", StringMarshalling = StringMarshalling.Utf16)]
    private static partial uint GetFinalPathNameByHandleW(nint file, [Out] char[] buffer, uint size, uint flags);

    [LibraryImport("kernel32.dll")]
    private static partial int CloseHandle(nint handle);

    /// <summary>Marker file Scripts/dev_build.py writes into its output folder: this install dir is a dev build.</summary>
    public const string DevBuildMarker = ".devbuild";

    /// <summary>Marker file the installer writes into an extracted install folder.</summary>
    public const string InstallMarker = ".install";

    /// <summary>Agent flag that lets a dev build open the default data folder anyway.</summary>
    public const string AllowDefaultHomeFlag = "--allow-default-home";

    /// <summary>A dev build never opens the user's real data folder (it would run dev code on their store) unless started
    /// with --allow-default-home or the folder holds the installer's .install marker: it needs its own BETTERRECALL_HOME.</summary>
    public static bool RefusesHome(string installDir, string home, bool allowDefaultHome)
    {
        if (allowDefaultHome || !DataPaths.IsDefaultHome(home) || !File.Exists(Path.Combine(installDir, DevBuildMarker))) return false;
        return !File.Exists(Path.Combine(installDir, InstallMarker));
    }
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

    /// <summary>True for the installed app's folder (%LOCALAPPDATA%\BetterRecall); false for a BETTERRECALL_HOME a dev
    /// build or test points elsewhere.</summary>
    public static bool IsDefaultHome(string home) => string.Equals(
        Path.GetFullPath(home).TrimEnd(Path.DirectorySeparatorChar, Path.AltDirectorySeparatorChar),
        Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.LocalApplicationData), "BetterRecall"),
        StringComparison.OrdinalIgnoreCase);

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
    /// <summary>Process id of the agent that owns this data folder (written once it holds AgentMutexFor(home)).</summary>
    public static string AgentPidFile(string home) => Path.Combine(home, "agent.pid");
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

    /// <summary>Base names a configured public pipe may not take: they belong to the UI, MCP and worker pipes.</summary>
    public static bool Reserved(string configuredBase)
    {
        var b = Base(configuredBase);
        return new[] { "BetterRecall.ui", "BetterRecall.mcp", "BetterRecall.worker" }
            .Any(r => b.StartsWith(r, StringComparison.OrdinalIgnoreCase));
    }

    /// <summary>Pipe names for the data folder home: the default folder keeps the plain names (older clients use them),
    /// any other folder gets the same suffix as AgentMutexFor, so a dev or test agent never serves the real UI, CLI or
    /// MCP client and theirs never reach the real agent.</summary>
    public static string Public(string configuredBase, string sid, string home) => Scoped($"{Base(configuredBase)}.{sid}", home);
    public static string Mcp(string sid, string home) => Scoped($"BetterRecall.mcp.{sid}", home);
    public static string Ui(string sid, string home) => Scoped($"BetterRecall.ui.{sid}", home);
    public static string NewWorker() => $"BetterRecall.worker.{Convert.ToHexString(System.Security.Cryptography.RandomNumberGenerator.GetBytes(16)).ToLowerInvariant()}";

    public const string AgentMutex = @"Local\BetterRecall.Agent";

    /// <summary>Auto-reset event the agent sets to show the launcher of an already running UI.</summary>
    public const string ShowLauncherEvent = @"Local\BetterRecall.UI.ShowLauncher";

    /// <summary>WinUI AppInstance key of the UI single instance.</summary>
    public const string UiInstanceKey = "BetterRecall.UI";

    /// <summary>Auto-reset event that asks the running agent to quit, as the tray Quit does. The single-file launcher
    /// sets it when a different version is already running, so the new one can take over.</summary>
    public const string AgentQuitEvent = @"Local\BetterRecall.Agent.Quit";

    /// <summary>Auto-reset event that asks the running UI to exit even with a window open. The single-file launcher sets
    /// it when it replaces another version: a UI the launcher started is not in the old agent's process tree, and a
    /// visible one outlived its agent, holding the old folder and the AppInstance key (C5).</summary>
    public const string UiQuitEvent = @"Local\BetterRecall.UI.Quit";

    /// <summary>Single-instance mutex and quit event for one data folder. The installed folder keeps the plain names
    /// (older launchers and agents use them); any other folder gets a suffix, so a dev or test agent never blocks or
    /// stops the user's real one.</summary>
    public static string AgentMutexFor(string home) => Scoped(AgentMutex, home);
    public static string AgentQuitEventFor(string home) => Scoped(AgentQuitEvent, home);
    public static string ShowLauncherEventFor(string home) => Scoped(ShowLauncherEvent, home);
    public static string UiInstanceKeyFor(string home) => Scoped(UiInstanceKey, home);
    public static string UiQuitEventFor(string home) => Scoped(UiQuitEvent, home);

    /// <summary>name for the default data folder, else name + "." + 16 hex of SHA-256 over the full upper-case path.</summary>
    public static string Scoped(string name, string home) => DataPaths.IsDefaultHome(home) ? name
        : name + "." + Convert.ToHexString(System.Security.Cryptography.SHA256.HashData(
            System.Text.Encoding.UTF8.GetBytes(Path.GetFullPath(home).TrimEnd(Path.DirectorySeparatorChar, Path.AltDirectorySeparatorChar).ToUpperInvariant())))[..16];
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
[JsonSerializable(typeof(RemovedEvent))]
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
[JsonSerializable(typeof(AiUsage))]
[JsonSerializable(typeof(ChatEvent))]
[JsonSerializable(typeof(ChatSource))]
[JsonSerializable(typeof(List<ChatSource>))]
[JsonSerializable(typeof(ChatSummary))]
[JsonSerializable(typeof(ItemsReply<ChatSummary>))]
[JsonSerializable(typeof(ChatDetail))]
[JsonSerializable(typeof(WorkerHello))]
[JsonSerializable(typeof(TranscribeResult))]
[JsonSerializable(typeof(BenchResult))]
[JsonSerializable(typeof(BenchReport))]
[JsonSerializable(typeof(WorkerRequest))]
[JsonSerializable(typeof(WorkerResponse))]
[JsonSerializable(typeof(ExportRequest))]
[JsonSerializable(typeof(ExportResult))]
[JsonSerializable(typeof(ModelComponent))]
[JsonSerializable(typeof(ModelsList))]
[JsonSerializable(typeof(ModelsProgress))]
[JsonSerializable(typeof(UpdateStatus))]
[JsonSerializable(typeof(UpdateProgress))]
[JsonSerializable(typeof(string))]
public partial class ContractsJson : JsonSerializerContext;
