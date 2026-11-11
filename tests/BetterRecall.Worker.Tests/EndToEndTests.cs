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

    /// <summary>artifacts\bin\&lt;project&gt;\&lt;same pivot as the test build&gt;\</summary>
    private static string BuildDir(string project)
    {
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
}
