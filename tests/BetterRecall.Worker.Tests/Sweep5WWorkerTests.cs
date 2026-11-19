// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Text.Json;
using BetterRecall.Contracts;
using BetterRecall.Worker.Engine;
using BetterRecall.Worker.Hosting;
using BetterRecall.Worker.Inference;
using BetterRecall.Worker.Inference.Onnx;
using BetterRecall.Worker.Models;
using BetterRecall.Worker.Protocol;

namespace BetterRecall.Worker.Tests;

/// <summary>Bug sweep 5, batch W: worker device cache on bench (A5) and worker log rotation/retention (C8).</summary>
public class Sweep5WWorkerTests
{
    private static WorkerReply Send(RequestHandler h, WorkerRequest r, byte[]? payload = null)
        => h.Handle(JsonSerializer.SerializeToUtf8Bytes(r, ContractsJson.Default.WorkerRequest), payload ?? []);

    // ---------------- A5: Bench persists device.json even when _failedDevices is non-empty ----------------

    [Fact]
    public void A5_Bench_updates_device_cache_when_failed_devices_is_non_empty()
    {
        var home = TestEnv.NewDir("s5w-a5");
        var models = Path.Combine(home, "models");
        TestEnv.InstallEmbeddingGemma(models);
        TestEnv.InstallSiglip(models);
        var s = TestEnv.Settings(home, x => { x.Inference.Device = "gpu"; x.Privacy.NsfwFilter = false; x.Ocr.Enabled = false; });

        using (var first = new RealEngine(s, home, new BenchFailBackend(), new OnnxBackend(cpuOnly: true)))
            Assert.Equal("synthetic/embeddinggemma-2", first.Hello(768).Model);

        var b = new BenchFailBackend();
        using (var engine = new RealEngine(s, home, b, new OnnxBackend(cpuOnly: true)))
        {
            var h = new RequestHandler(engine, () => s);
            Assert.Equal("synthetic/embeddinggemma-2", Send(h, new WorkerRequest(1, "hello")).Header.Hello!.Model);

            b.GpuBroken = true;
            for (int i = 0; i < RealEngine.RunFailuresBeforeReplan; i++)
                Assert.False(Send(h, new WorkerRequest(2 + i, "embed_text", Text: "lost", Kind: "query")).Header.Ok);
            Assert.Contains("GPU.0", engine.FailedDevices);

            // EmbeddingGemma no longer runs on CPU either; user bench picks SigLIP on CPU and must persist it.
            b.EgBroken = true;
            var bench = Send(h, new WorkerRequest(10, "bench")).Header;
            Assert.True(bench.Ok, bench.Error);
            Assert.Equal("synthetic/siglip2-base", bench.Model);
            Assert.Equal("CPU", bench.Device);
        }

        var cache = JsonSerializer.Deserialize(File.ReadAllBytes(DataPaths.DeviceCache(home)), WorkerJson.Default.DeviceCacheFile)!;
        Assert.Equal(ModelCatalog.Siglip, cache.Chosen!.Model);
        Assert.Equal("CPU", cache.Chosen.DeviceId);

        // Next worker starts directly on the persisted choice instead of thrashing back to EmbeddingGemma.
        using var next = new RealEngine(s, home, new BenchFailBackend(), new OnnxBackend(cpuOnly: true));
        var nextHello = next.Hello(768);
        Assert.Equal("synthetic/siglip2-base", nextHello.Model);
        Assert.Equal("CPU", nextHello.Device);
    }

    [Fact]
    public void A5_Bench_retests_failed_accelerator_and_updates_device_cache_on_recovery()
    {
        var home = TestEnv.NewDir("s5w-a5-recover");
        var models = Path.Combine(home, "models");
        TestEnv.InstallEmbeddingGemma(models);
        var s = TestEnv.Settings(home, x => { x.Inference.Device = "gpu"; x.Privacy.NsfwFilter = false; x.Ocr.Enabled = false; });

        var b = new BenchFailBackend();
        using var engine = new RealEngine(s, home, b, new OnnxBackend(cpuOnly: true));
        Assert.Equal("GPU", engine.Hello(768).Device);

        b.GpuBroken = true;
        for (int i = 0; i < RealEngine.RunFailuresBeforeReplan; i++)
            Assert.Throws<DeviceFailedException>(() => engine.EmbedText("lost", TextKind.Query));
        Assert.Contains("GPU.0", engine.FailedDevices);

        // Replan falls back to CPU while device.json still holds GPU.0; overwrite cache to CPU to prove Bench rewrites it.
        Assert.NotEmpty(engine.EmbedText("cpu-fallback", TextKind.Query));
        var cachePath = DataPaths.DeviceCache(home);
        var stale = JsonSerializer.Deserialize(File.ReadAllBytes(cachePath), WorkerJson.Default.DeviceCacheFile)!;
        stale.Chosen = new DeviceChoice("CPU", "CPU", "OpenVINO", ModelCatalog.EmbeddingGemma);
        DeviceCache.Write(cachePath, stale);

        b.GpuBroken = false;
        var bench = engine.Bench();
        Assert.Equal("GPU", bench.Chosen);
        Assert.DoesNotContain("GPU.0", engine.FailedDevices);

        var updated = JsonSerializer.Deserialize(File.ReadAllBytes(cachePath), WorkerJson.Default.DeviceCacheFile)!;
        Assert.Equal("GPU.0", updated.Chosen!.DeviceId);
    }

    // ---------------- C8: worker log rotation at 10 MiB and 14-file retention ----------------

    [Fact]
    public void C8_Worker_log_rotates_at_10MiB_and_prunes_beyond_14_files()
    {
        var dir = TestEnv.NewDir("s5w-log");
        var now = new DateTimeOffset(DateTimeOffset.UtcNow.Date.AddHours(10), TimeSpan.Zero);
        var oldNames = new List<string>();
        for (int d = 1; d <= 18; d++)
        {
            var oldDate = now.AddDays(-30 + d);
            var name = $"worker-{oldDate:yyyyMMdd}.log";
            oldNames.Add(name);
            var old = Path.Combine(dir, name);
            File.WriteAllText(old, "old\n");
            File.SetLastWriteTimeUtc(old, oldDate.UtcDateTime);
        }

        var todayFile = Path.Combine(dir, $"worker-{now:yyyyMMdd}.log");
        using (var fs = File.Create(todayFile)) fs.SetLength(Log.MaxFileBytes);
        File.SetLastWriteTimeUtc(todayFile, now.UtcDateTime);

        var sink = new LogSink(() => now);
        sink.Init(dir, "debug");

        // Init pruned the 19 files down to the 14 newest (oldest 5 deleted, 6..18 + today kept).
        var afterInit = Directory.GetFiles(dir, "worker-*.log").Select(Path.GetFileName).OrderBy(x => x).ToArray();
        Assert.Equal(Log.KeepFiles, afterInit.Length);
        Assert.DoesNotContain(oldNames[4], afterInit);
        Assert.Contains(oldNames[5], afterInit);

        // First write rotates away from the oversized daily file and keeps total files capped at 14.
        sink.Write(0, "DBG", "first-rotated");
        var firstRot = Path.Combine(dir, $"worker-{now:yyyyMMdd}-100000.log");
        Assert.True(File.Exists(firstRot));
        Assert.Contains("first-rotated", File.ReadAllText(firstRot));
        Assert.Equal(Log.MaxFileBytes, new FileInfo(todayFile).Length);
        var afterFirstWrite = Directory.GetFiles(dir, "worker-*.log").Select(Path.GetFileName).OrderBy(x => x).ToArray();
        Assert.Equal(Log.KeepFiles, afterFirstWrite.Length);
        Assert.DoesNotContain(oldNames[5], afterFirstWrite);

        // Growing the rotated file past 10 MiB rotates again cleanly without exceeding 14 files.
        using (var fs = File.OpenWrite(firstRot)) fs.SetLength(Log.MaxFileBytes);
        now = now.AddSeconds(1);
        sink.Write(1, "INF", "second-rotated");
        var secondRot = Path.Combine(dir, $"worker-{now:yyyyMMdd}-100001.log");
        Assert.True(File.Exists(secondRot));
        Assert.Contains("second-rotated", File.ReadAllText(secondRot));
        Assert.Equal(Log.KeepFiles, Directory.GetFiles(dir, "worker-*.log").Length);
    }

    [Fact]
    public void C8_Static_Log_facade_writes_and_prunes_beyond_14_files()
    {
        var dir = TestEnv.NewDir("s5w-log-static");
        var today = DateTimeOffset.UtcNow.Date;
        string? oldestPath = null;
        for (int d = 1; d <= 16; d++)
        {
            var oldDate = today.AddDays(-30 + d);
            var old = Path.Combine(dir, $"worker-{oldDate:yyyyMMdd}.log");
            oldestPath ??= old;
            File.WriteAllText(old, "old\n");
            File.SetLastWriteTimeUtc(old, oldDate);
        }

        try
        {
            Log.Init(dir, "info");
            Log.Info("static-probe");
            var files = Directory.GetFiles(dir, "worker-*.log");
            Assert.Equal(Log.KeepFiles, files.Length);
            Assert.False(File.Exists(oldestPath!));
        }
        finally
        {
            Log.Init(null, "info");
        }
    }

    private sealed class BenchFailBackend : IInferenceBackend
    {
        private readonly Sweep2GTests.FlakyGpuBackend _inner = new();
        public volatile bool EgBroken;
        public bool GpuBroken { set => _inner.Broken = value; }
        public string Name => _inner.Name;
        public string Version => _inner.Version;
        public string NpuDriverVersion => _inner.NpuDriverVersion;
        public IReadOnlyList<DeviceInfo> Devices => _inner.Devices;

        public IInferenceSession Load(string modelPath, DeviceInfo device, string? cacheDir, IReadOnlyDictionary<string, long[]>? staticShapes = null)
        {
            if (EgBroken && modelPath.Contains(ModelCatalog.EmbeddingGemma, StringComparison.OrdinalIgnoreCase))
                throw new DeviceFailedException("runtime broken");
            return _inner.Load(modelPath, device, cacheDir, staticShapes);
        }

        public void Dispose() => _inner.Dispose();
    }
}
