// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Diagnostics;
using System.IO.Pipes;
using BetterRecall.Contracts;
using Xunit.Abstractions;

namespace BetterRecall.Worker.Tests;

/// <summary>Copies the worker build and FakeAgent layout once per test class instead of per test.</summary>
public sealed class WorkerLayoutFixture : IDisposable
{
    private readonly TestEnv.ScopedDir _scratch;

    public string InstallDir { get; }
    public string AgentExe => Path.Combine(InstallDir, InstallLayout.AgentExe);
    public string WorkerExe => InstallLayout.Full(InstallDir, InstallLayout.WorkerExe);
    public string DirectWorkerExe => Path.Combine(BuildDir("BetterRecall.Worker"), "BetterRecall.Worker.exe");
    public long CopyMs { get; }

    public WorkerLayoutFixture()
    {
        _scratch = TestEnv.Scoped("e2e-layout");
        InstallDir = Path.Combine(_scratch.Path, "install");
        var sw = Stopwatch.StartNew();
        CopyTree(BuildDir("BetterRecall.Worker"), Path.Combine(InstallDir, "worker"));
        CopyTree(BuildDir("FakeAgent"), InstallDir);
        File.Move(Path.Combine(InstallDir, "FakeAgent.exe"), AgentExe);
        CopyMs = sw.ElapsedMilliseconds;
    }

    public void Dispose() => _scratch.Dispose();

    private static string RepoRoot()
    {
        var d = new DirectoryInfo(AppContext.BaseDirectory);
        while (d is not null && !File.Exists(Path.Combine(d.FullName, "BetterRecall.slnx"))) d = d.Parent;
        return d?.FullName ?? throw new InvalidOperationException("repo root not found");
    }

    /// <summary>artifacts\bin\&lt;project&gt;\&lt;same pivot as the test build&gt;\, or BR_WORKER_DIR for the worker.</summary>
    private static string BuildDir(string project)
    {
        if (project == "BetterRecall.Worker"
            && Environment.GetEnvironmentVariable("BR_WORKER_DIR") is { } workerDir
            && !string.IsNullOrWhiteSpace(workerDir))
            return workerDir;
        var pivot = new DirectoryInfo(AppContext.BaseDirectory.TrimEnd('\\')).Name;
        var dir = Path.Combine(RepoRoot(), "artifacts", "bin", project, pivot);
        if (!Directory.Exists(dir)) dir = Directory.GetDirectories(Path.Combine(RepoRoot(), "artifacts", "bin", project)).First();
        return dir;
    }

    private static void CopyTree(string from, string to)
    {
        Directory.CreateDirectory(to);
        foreach (var f in Directory.GetFiles(from, "*", SearchOption.AllDirectories))
        {
            var dst = Path.Combine(to, Path.GetRelativePath(from, f));
            Directory.CreateDirectory(Path.GetDirectoryName(dst)!);
            File.Copy(f, dst, true);
        }
    }
}

/// <summary>Launches the real BetterRecall.Worker.exe. The positive case lays out &lt;install&gt;\BetterRecall.Agent.exe
/// (the FakeAgent helper under that name) and &lt;install&gt;\worker\ (a copy of the worker build), so the worker's
/// server-image check passes for the right reason.</summary>
public class EndToEndTests(WorkerLayoutFixture layout, ITestOutputHelper output) : IClassFixture<WorkerLayoutFixture>
{
    [Fact]
    public void Worker_exe_fake_mode_end_to_end_with_hardening()
    {
        using var scratch = TestEnv.Scoped("e2e");
        var root = scratch.Path;
        output.WriteLine($"layout copied in {layout.CopyMs} ms");

        // Seed 16 old logs + an oversized (10 MiB) today log so startup exercises C8 pruning & rotation.
        var home = Path.Combine(root, "home");
        TestEnv.Settings(home, s => { s.Inference.Fake = true; s.Security.MemoryIsolation = true; s.Governance.EcoQos = true; s.Index.Dim = 256; s.Log.Level = "debug"; });
        var before = DateTime.Now;
        var logsDir = DataPaths.LogsDir(home);
        Directory.CreateDirectory(logsDir);
        string? oldest = null;
        for (int i = 1; i <= 16; i++)
        {
            var oldDate = before.AddDays(-30 + i);
            var old = Path.Combine(logsDir, $"worker-{oldDate:yyyyMMdd}.log");
            oldest ??= old;
            File.WriteAllText(old, "old\n");
            File.SetLastWriteTimeUtc(old, oldDate.ToUniversalTime());
        }
        var oversized = Path.Combine(logsDir, $"worker-{before:yyyyMMdd}.log");
        using (var fs = File.Create(oversized)) fs.SetLength(10L * 1024 * 1024);
        File.SetLastWriteTimeUtc(oversized, before.ToUniversalTime());

        var outFile = Path.Combine(root, "out.txt");
        var psi = new ProcessStartInfo(layout.AgentExe,
            [layout.WorkerExe, home, outFile]) { UseShellExecute = false };
        using var agent = Process.Start(psi)!;
        Assert.True(agent.WaitForExit(90_000), "fake agent timed out");
        var lines = File.ReadAllLines(outFile);
        foreach (var l in lines) output.WriteLine(l);
        Assert.Equal(7, lines.Length);
        Assert.Contains("\"ep\":\"fake\"", lines[0]);
        Assert.Contains("\"dim\":256", lines[1]);
        Assert.EndsWith("payload=1024", lines[1]);
        Assert.EndsWith("payload=1024", lines[3]);
        Assert.Contains("\"speech\":true", lines[4]);
        Assert.Contains("\"ok\":true", lines[5]);
        Assert.Equal("exit=0", lines[6]);

        var files = Directory.GetFiles(logsDir, "worker-*.log");
        Assert.Equal(14, files.Length);
        Assert.False(File.Exists(oldest!));
        var activeLog = DateTime.Now.Date == before.Date
            ? files.Single(f => Path.GetFileName(f).StartsWith($"worker-{before:yyyyMMdd}-", StringComparison.Ordinal))
            : files.OrderByDescending(File.GetLastWriteTimeUtc).First();
        var text = File.ReadAllText(activeLog);
        output.WriteLine(text);
        Assert.Contains("start settings=", text);
        Assert.Contains("hardening applied=[", text);
        Assert.DoesNotContain("hello world", text); // no content in logs

        // Sweep 4 C5 + A-M3: agent settings on stdin win over settings.json, and bench/embed replies name the embedder.
        var spaceHome = Path.Combine(root, "home-space");
        TestEnv.Settings(spaceHome, s => { s.Inference.Fake = false; s.Index.Dim = 768; s.Security.MemoryIsolation = false; });
        const string agentKeys = """{"inference":{"fake":true},"index":{"dim":512},"security":{"memoryIsolation":false}}""";
        var spaceOut = Path.Combine(root, "out-space.txt");
        var spacePsi = new ProcessStartInfo(layout.AgentExe,
            [layout.WorkerExe, spaceHome, spaceOut, "--space", "--settings", agentKeys]) { UseShellExecute = false };
        using var spaceAgent = Process.Start(spacePsi)!;
        Assert.True(spaceAgent.WaitForExit(90_000), "fake agent (--space) timed out");
        var spaceLines = File.ReadAllLines(spaceOut);
        foreach (var l in spaceLines) output.WriteLine(l);
        Assert.Equal(5, spaceLines.Length);
        Assert.Contains("\"ep\":\"fake\"", spaceLines[0]);
        Assert.Contains("\"dim\":512", spaceLines[0]);
        Assert.Contains("\"model\":\"fake/embeddinggemma-2\"", spaceLines[1]);  // bench
        Assert.Contains("\"device\":\"CPU\"", spaceLines[1]);
        Assert.Contains("\"model\":\"fake/embeddinggemma-2\"", spaceLines[2]);  // embed_text
        Assert.Contains("\"revision\":\"fake-1\"", spaceLines[2]);
        Assert.EndsWith("payload=2048", spaceLines[2]);                         // 512 floats
        Assert.Equal("exit=0", spaceLines[4]);
        var spaceLog = File.ReadAllText(Directory.GetFiles(DataPaths.LogsDir(spaceHome), "worker-*.log").Single());
        Assert.Contains("start settings=agent ", spaceLog);
        Assert.Contains(" fake=True ", spaceLog);
    }

    /// <summary>SF1 (review): --settings-stdin with input that is missing, not JSON or too large: the real worker exe
    /// exits with 2 (bad arguments or settings) instead of running on defaults.</summary>
    [Theory]
    [InlineData("missing")]
    [InlineData("garbage")]
    [InlineData("huge")]
    public void Worker_exe_exits_on_bad_settings_input(string kind)
    {
        using var scratch = TestEnv.Scoped("s4w-bad");
        var home = Path.Combine(scratch.Path, "home");
        var psi = new ProcessStartInfo(layout.DirectWorkerExe,
            ["--pipe", PipeNames.NewWorker(), "--home", home, "--settings-stdin"]) { UseShellExecute = false, RedirectStandardInput = true };
        using var worker = Process.Start(psi)!;
        byte[] input = kind switch
        {
            "garbage" => "{ not json"u8.ToArray(),
            "huge" => System.Text.Encoding.UTF8.GetBytes("{\"log\":{\"level\":\"" + new string('a', 70_000) + "\"}}"),
            _ => [],
        };
        try { worker.StandardInput.BaseStream.Write(input); worker.StandardInput.Close(); } catch (IOException) { }
        Assert.True(worker.WaitForExit(30_000), "worker did not exit");
        Assert.Equal(2, worker.ExitCode);
        var log = File.ReadAllText(Directory.GetFiles(DataPaths.LogsDir(home), "worker-*.log").Single());
        Assert.Contains("settings from the agent", log);
        Assert.DoesNotContain("start settings=", log);
    }

    /// <summary>Real mode in the real exe with memory isolation on: OpenVINO, ONNX Runtime and Skia must still load
    /// under the mitigation policies (child processes blocked, strict handles, DLL search limited to app dir + System32).</summary>
    [Fact]
    public void Worker_exe_real_mode_loads_runtimes_under_hardening()
    {
        using var scratch = TestEnv.Scoped("e2ereal");
        var root = scratch.Path;
        var home = Path.Combine(root, "home");
        TestEnv.Settings(home, s => { s.Inference.Fake = false; s.Security.MemoryIsolation = true; s.Index.Dim = 768; });
        var models = Path.Combine(home, "models");
        TestEnv.InstallEmbeddingGemma(models);
        TestEnv.InstallNsfw(models);
        TestEnv.InstallVad(models);
        var outFile = Path.Combine(root, "out.txt");
        using var agent = Process.Start(new ProcessStartInfo(layout.AgentExe,
            [layout.WorkerExe, home, outFile]) { UseShellExecute = false })!;
        Assert.True(agent.WaitForExit(180_000), "fake agent timed out");
        var lines = File.ReadAllLines(outFile);
        foreach (var l in lines) output.WriteLine(l);
        output.WriteLine(File.ReadAllText(Directory.GetFiles(DataPaths.LogsDir(home), "worker-*.log").Single()));
        Assert.Contains("\"ep\":\"OpenVINO\"", lines[0]);
        Assert.EndsWith("payload=3072", lines[1]);
        Assert.EndsWith("payload=3072", lines[3]);
        Assert.Contains("Model not installed: whisper-large-v3-turbo.", lines[4]);
        Assert.Equal("exit=0", lines[6]);
    }

    [Fact]
    public async Task Worker_exe_rejects_a_pipe_server_that_is_not_the_agent()
    {
        var exe = layout.DirectWorkerExe;
        var home = TestEnv.NewDir("reject");
        TestEnv.Settings(home, s => s.Inference.Fake = true);
        var name = PipeNames.NewWorker();
        await using var server = new NamedPipeServerStream(name, PipeDirection.InOut, 1, PipeTransmissionMode.Byte, PipeOptions.Asynchronous);
        using var p = Process.Start(new ProcessStartInfo(exe, ["--pipe", name, "--home", home]) { UseShellExecute = false })!;
        await server.WaitForConnectionAsync().WaitAsync(TimeSpan.FromSeconds(30));
        Assert.True(p.WaitForExit(30_000));
        Assert.Equal(4, p.ExitCode);

        using var bad = Process.Start(new ProcessStartInfo(exe, ["--pipe", "not-a-worker-pipe", "--home", home]) { UseShellExecute = false })!;
        Assert.True(bad.WaitForExit(30_000));
        Assert.Equal(2, bad.ExitCode);
    }
}
