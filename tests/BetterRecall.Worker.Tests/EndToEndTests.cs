// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Diagnostics;
using System.IO.Pipes;
using BetterRecall.Contracts;
using Xunit.Abstractions;

namespace BetterRecall.Worker.Tests;

/// <summary>Launches the real BetterRecall.Worker.exe. The positive case lays out &lt;install&gt;\BetterRecall.Agent.exe
/// (the FakeAgent helper under that name) and &lt;install&gt;\worker\ (a copy of the worker build), so the worker's
/// server-image check passes for the right reason.</summary>
public class EndToEndTests(ITestOutputHelper output)
{
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

    [Fact]
    public void Worker_exe_fake_mode_end_to_end_with_hardening()
    {
        using var scratch = TestEnv.Scoped("e2e");
        var root = scratch.Path;
        var install = Path.Combine(root, "install");
        var sw = Stopwatch.StartNew();
        CopyTree(BuildDir("BetterRecall.Worker"), Path.Combine(install, "worker"));
        CopyTree(BuildDir("FakeAgent"), install);
        File.Move(Path.Combine(install, "FakeAgent.exe"), Path.Combine(install, InstallLayout.AgentExe));
        output.WriteLine($"layout copied in {sw.ElapsedMilliseconds} ms");

        var home = Path.Combine(root, "home");
        TestEnv.Settings(home, s => { s.Inference.Fake = true; s.Security.MemoryIsolation = true; s.Governance.EcoQos = true; s.Index.Dim = 256; });
        var outFile = Path.Combine(root, "out.txt");
        var psi = new ProcessStartInfo(Path.Combine(install, InstallLayout.AgentExe),
            [InstallLayout.Full(install, InstallLayout.WorkerExe), home, outFile]) { UseShellExecute = false };
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

        var log = Directory.GetFiles(DataPaths.LogsDir(home), "worker-*.log").Single();
        var text = File.ReadAllText(log);
        output.WriteLine(text);
        Assert.Contains("hardening applied=[", text);
        Assert.DoesNotContain("hello world", text); // no content in logs
    }

    /// <summary>Sweep 4 C5 + A-M3 seam, real worker exe in fake mode: the agent's worker keys come on stdin and win
    /// over a settings.json the agent never loaded (here: not fake, dim 768), and every embed and bench reply
    /// names the embedder that served it.</summary>
    [Fact]
    public void Worker_exe_runs_on_the_agents_settings_and_names_its_embedder()
    {
        using var scratch = TestEnv.Scoped("e2e-s4w");
        var root = scratch.Path;
        var install = Path.Combine(root, "install");
        CopyTree(BuildDir("BetterRecall.Worker"), Path.Combine(install, "worker"));
        CopyTree(BuildDir("FakeAgent"), install);
        File.Move(Path.Combine(install, "FakeAgent.exe"), Path.Combine(install, InstallLayout.AgentExe));

        var home = Path.Combine(root, "home");
        TestEnv.Settings(home, s => { s.Inference.Fake = false; s.Index.Dim = 768; s.Security.MemoryIsolation = false; });   // a hand edit
        const string agentKeys = """{"inference":{"fake":true},"index":{"dim":512},"security":{"memoryIsolation":false}}""";
        var outFile = Path.Combine(root, "out.txt");
        var psi = new ProcessStartInfo(Path.Combine(install, InstallLayout.AgentExe),
            [InstallLayout.Full(install, InstallLayout.WorkerExe), home, outFile, "--space", "--settings", agentKeys]) { UseShellExecute = false };
        using var agent = Process.Start(psi)!;
        Assert.True(agent.WaitForExit(90_000), "fake agent timed out");
        var lines = File.ReadAllLines(outFile);
        foreach (var l in lines) output.WriteLine(l);
        Assert.Equal(5, lines.Length);
        Assert.Contains("\"ep\":\"fake\"", lines[0]);                       // fake from the agent, not the file
        Assert.Contains("\"dim\":512", lines[0]);
        Assert.Contains("\"model\":\"fake/embeddinggemma-2\"", lines[1]);  // bench
        Assert.Contains("\"device\":\"CPU\"", lines[1]);
        Assert.Contains("\"model\":\"fake/embeddinggemma-2\"", lines[2]);  // embed_text
        Assert.Contains("\"revision\":\"fake-1\"", lines[2]);
        Assert.EndsWith("payload=2048", lines[2]);                         // 512 floats
        Assert.Equal("exit=0", lines[4]);
        var log = File.ReadAllText(Directory.GetFiles(DataPaths.LogsDir(home), "worker-*.log").Single());
        Assert.Contains("start settings=agent ", log);
        Assert.Contains(" fake=True ", log);
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
        var psi = new ProcessStartInfo(Path.Combine(BuildDir("BetterRecall.Worker"), "BetterRecall.Worker.exe"),
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
        var install = Path.Combine(root, "install");
        CopyTree(BuildDir("BetterRecall.Worker"), Path.Combine(install, "worker"));
        CopyTree(BuildDir("FakeAgent"), install);
        File.Move(Path.Combine(install, "FakeAgent.exe"), Path.Combine(install, InstallLayout.AgentExe));
        var home = Path.Combine(root, "home");
        TestEnv.Settings(home, s => { s.Inference.Fake = false; s.Security.MemoryIsolation = true; s.Index.Dim = 768; });
        var models = Path.Combine(home, "models");
        TestEnv.InstallEmbeddingGemma(models);
        TestEnv.InstallNsfw(models);
        TestEnv.InstallVad(models);
        var outFile = Path.Combine(root, "out.txt");
        using var agent = Process.Start(new ProcessStartInfo(Path.Combine(install, InstallLayout.AgentExe),
            [InstallLayout.Full(install, InstallLayout.WorkerExe), home, outFile]) { UseShellExecute = false })!;
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
        var exe = Path.Combine(BuildDir("BetterRecall.Worker"), "BetterRecall.Worker.exe");
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

    /// <summary>C8 seam: real BetterRecall.Worker.exe launched by FakeAgent prunes old worker logs beyond 14 files and
    /// rotates an oversized today log at startup.</summary>
    [Fact]
    public void Worker_exe_prunes_old_logs_and_rotates_oversized_daily_log()
    {
        using var scratch = TestEnv.Scoped("e2e-c8");
        var root = scratch.Path;
        var install = Path.Combine(root, "install");
        CopyTree(BuildDir("BetterRecall.Worker"), Path.Combine(install, "worker"));
        CopyTree(BuildDir("FakeAgent"), install);
        File.Move(Path.Combine(install, "FakeAgent.exe"), Path.Combine(install, InstallLayout.AgentExe));

        var home = Path.Combine(root, "home");
        TestEnv.Settings(home, s => { s.Inference.Fake = true; s.Log.Level = "debug"; });
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
        var psi = new ProcessStartInfo(Path.Combine(install, InstallLayout.AgentExe),
            [InstallLayout.Full(install, InstallLayout.WorkerExe), home, outFile, "--space"]) { UseShellExecute = false };
        using var agent = Process.Start(psi)!;
        Assert.True(agent.WaitForExit(90_000), "fake agent timed out");
        Assert.Equal("exit=0", File.ReadAllLines(outFile).Last());

        var files = Directory.GetFiles(logsDir, "worker-*.log");
        Assert.Equal(14, files.Length);
        Assert.False(File.Exists(oldest!));
        if (DateTime.Now.Date == before.Date)
        {
            var rotated = files.Single(f => Path.GetFileName(f).StartsWith($"worker-{before:yyyyMMdd}-", StringComparison.Ordinal));
            Assert.Contains("start settings=", File.ReadAllText(rotated));
        }
    }
}
