// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Runtime.CompilerServices;
using System.Security.Cryptography;
using BetterRecall.Contracts;
using BetterRecall.Core;
using BetterRecall.Core.Store;

namespace BetterRecall.Core.Tests;

internal static class TestEnv
{
    /// <summary>BR_TEST_TMP (set by a local runsettings file) plus "betterrecall-backend", else the temp dir.</summary>
    public static readonly string Root = Path.Combine(
        Environment.GetEnvironmentVariable("BR_TEST_TMP") is { Length: > 0 } t ? t : Path.GetTempPath(), "betterrecall-backend");

    [ModuleInitializer]
    internal static void Init()
    {
        Directory.CreateDirectory(Root);
        var home = Path.Combine(Root, "home-default");
        Environment.SetEnvironmentVariable(DataPaths.HomeEnv, home);
        Log.Init(Path.Combine(Root, "logs"), "tests");
        Log.Level = LogLevel.Debug;
    }
}

/// <summary>Fresh BETTERRECALL_HOME under TestEnv.Root with dpapi key protection. Deleted on dispose.</summary>
internal sealed class TempHome : IDisposable
{
    public string Path { get; }

    public TempHome(string? keyProtection = "dpapi")
    {
        Path = System.IO.Path.Combine(TestEnv.Root, "h-" + Guid.NewGuid().ToString("N")[..12]);
        Directory.CreateDirectory(Path);
        if (keyProtection != null)
            File.WriteAllText(DataPaths.SettingsFile(Path), "{\"security\":{\"keyProtection\":\"" + keyProtection + "\"}}");
    }

    public void Dispose()
    {
        Microsoft.Data.Sqlite.SqliteConnection.ClearAllPools();
        for (int i = 0; i < 5; i++)
        {
            try { if (Directory.Exists(Path)) Directory.Delete(Path, true); return; }
            catch (IOException) { Thread.Sleep(100); }
            catch (UnauthorizedAccessException) { Thread.Sleep(100); }
        }
    }
}

/// <summary>Store with a random raw key (no vault).</summary>
internal sealed class TempStore : IDisposable
{
    public readonly TempHome Home = new(null);
    public readonly byte[] Key = RandomNumberGenerator.GetBytes(32);
    public EncryptedStore Store { get; private set; }

    public TempStore() => Store = Open();

    public EncryptedStore Open() => new(Home.Path, () => (byte[])Key.Clone());

    public void Reopen()
    {
        Store.Dispose();
        Store = Open();
    }

    public void Dispose()
    {
        Store.Dispose();
        Home.Dispose();
    }
}

internal static class Vec
{
    public static readonly EmbeddingInfo Info = new("fake-gemma", "r1", 768);

    /// <summary>Deterministic unit vector from a seed.</summary>
    public static float[] Seeded(int seed, int dim = 768)
    {
        var r = new Random(seed);
        var v = new float[dim];
        double n = 0;
        for (int i = 0; i < dim; i++) { v[i] = (float)(r.NextDouble() * 2 - 1); n += v[i] * v[i]; }
        float inv = (float)(1 / Math.Sqrt(n));
        for (int i = 0; i < dim; i++) v[i] *= inv;
        return v;
    }

    /// <summary>v plus a little noise: close to v.</summary>
    public static float[] Near(float[] v, int seed, float noise = 0.15f)
    {
        var r = new Random(seed);
        var o = new float[v.Length];
        for (int i = 0; i < v.Length; i++) o[i] = v[i] + (float)(r.NextDouble() * 2 - 1) * noise / MathF.Sqrt(v.Length) * 4;
        return o;
    }
}

internal static class Caps
{
    public static readonly byte[] FakeWebp = [.. "RIFF"u8, 0, 0, 0, 0, .. "WEBPVP8 MARKER-PIXELS-0123456789"u8];

    public static NewScreenCapture Screen(DateTimeOffset ts, string app = "chrome.exe", string title = "Some window", bool keep = true, byte[]? image = null)
        => new(ts, app, @"C:\Apps\" + app, title, 0, 1280, 720, 0x1234, image ?? FakeWebp, "image/webp", keep);

    public static NewAudioCapture Audio(DateTimeOffset ts, string text, string app = "teams.exe")
        => new(ts, "system", 4200, text, "en", app);
}

internal sealed class FakeAgent : IAgentControl
{
    public bool Unlocked = true;
    public bool UnlockResult = true;
    public List<string> Calls = [];
    public AgentStatus Status = new(true, false, null, null, false, null, new AudioState(false, false), new QueueState(0), "CPU",
        new StoreStats(0, 0, 0, null, null, 0, 0), 30);
    public List<ReplayClip> Replays = [];

    public AgentStatus GetStatus() => Status;
    public void Pause(TimeSpan? duration, string reason) => Calls.Add($"pause {duration?.TotalSeconds} {reason}");
    public void Resume(string reason) => Calls.Add($"resume {reason}");
    public bool IsUnlocked => Unlocked;
    public Task<bool> UnlockAsync(CancellationToken ct) { Calls.Add("unlock"); if (UnlockResult) Unlocked = true; return Task.FromResult(UnlockResult); }
    public void Lock() { Calls.Add("lock"); Unlocked = false; }
    public void OpenReadGate() { Calls.Add("opengate"); Unlocked = true; }
    public void Consent() => Calls.Add("consent");
    public Task<ReplayClip> SaveReplayAsync(CancellationToken ct)
    {
        var c = new ReplayClip("r1", DateTimeOffset.UtcNow, 120000, 1000, true, @"C:\x.mp4.enc");
        Replays.Add(c);
        return Task.FromResult(c);
    }
    public IReadOnlyList<ReplayClip> ListReplays() => Replays;
    public Task ExportReplayAsync(string id, string path, CancellationToken ct)
    {
        Calls.Add($"export {id} {path}");
        return Replays.Any(r => r.Id == id) ? Task.CompletedTask : throw new KeyNotFoundException();
    }
    public void DeleteReplay(string id) { if (Replays.RemoveAll(r => r.Id == id) == 0) throw new KeyNotFoundException(); }
    public void Panic() => Calls.Add("panic");
    public Task<BenchResult> BenchAsync(CancellationToken ct) => Task.FromResult(new BenchResult("CPU", [new BenchDevice("CPU", "fake", true, 1, 1, 1, null)]));
    public BenchReport? FullReport;
    public bool BlockFullBench;
    public TaskCompletionSource FullBenchCancelled = new(TaskCreationOptions.RunContinuationsAsynchronously);
    public TaskCompletionSource FullBenchStarted = new(TaskCreationOptions.RunContinuationsAsynchronously);
    public async Task<BenchReport> BenchFullAsync(CancellationToken ct)
    {
        FullBenchStarted.TrySetResult();
        if (BlockFullBench)
        {
            try { await Task.Delay(Timeout.Infinite, ct); }
            catch (OperationCanceledException) { FullBenchCancelled.TrySetResult(); throw; }
        }
        return FullReport ?? throw new NotSupportedException();
    }
    public event Action<AgentStatus>? StatusChanged;
    public event Action<CaptureMeta>? CaptureStored;
    public void RaiseStatus() => StatusChanged?.Invoke(Status);
    public void RaiseCapture(CaptureMeta m) => CaptureStored?.Invoke(m);
}

internal sealed class FakeEmbedder : IQueryEmbedder
{
    public Dictionary<string, float[]> Map = new(StringComparer.Ordinal);
    public bool Fail;
    public int Calls;
    public Task<float[]> EmbedQueryAsync(string text, CancellationToken ct)
    {
        Calls++;
        if (Fail) throw new InvalidOperationException("worker down");
        return Task.FromResult(Map.TryGetValue(text, out var v) ? v : Vec.Seeded(text.GetHashCode()));
    }
}

internal sealed class FakeThumbs : IThumbnailer
{
    public int Cleared;
    public Task<ThumbData?> GetThumbAsync(string id, int width, CancellationToken ct)
        => Task.FromResult(id == "missing" ? null : new ThumbData(id, "image/webp", width, width * 9 / 16, [1, 2, 3]));
    public void ClearCache() => Cleared++;
}
