// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Diagnostics;
using BetterRecall.Contracts;
using Xunit.Abstractions;

namespace BetterRecall.Worker.Tests;

public class RealModelTests(ITestOutputHelper output)
{
    private static string RepoRoot()
    {
        var d = new DirectoryInfo(AppContext.BaseDirectory);
        while (d is not null && !File.Exists(Path.Combine(d.FullName, "BetterRecall.slnx"))) d = d.Parent;
        return d?.FullName ?? throw new InvalidOperationException("repo root not found");
    }

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

    /// <summary>Test data folder: BR_REAL_HOME when set (keeps the compiled-model cache between runs), else a fresh
    /// TestEnv folder. Never the installed app's data folder: these tests overwrite settings.json and delete caches.</summary>
    private static string TestHome()
    {
        var home = Environment.GetEnvironmentVariable("BR_REAL_HOME");
        if (string.IsNullOrWhiteSpace(home)) return TestEnv.NewDir("real-home");
        var real = Path.GetFullPath(Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.LocalApplicationData), "BetterRecall"));
        Assert.False(string.Equals(Path.GetFullPath(home).TrimEnd('\\'), real, StringComparison.OrdinalIgnoreCase),
            "BR_REAL_HOME points at the installed app's data folder; use a separate folder (its settings.json would be overwritten).");
        return home;
    }

    [RequiresEnvFact("BR_REAL_MODELS", "BR_REAL_WORKER_DIR")]
    public void Real_published_worker_exe_under_fake_agent()
    {
        var models = Environment.GetEnvironmentVariable("BR_REAL_MODELS");
        if (string.IsNullOrWhiteSpace(models) || !Directory.Exists(models))
        {
            output.WriteLine("skipped: BR_REAL_MODELS not set");
            return;
        }

        TestHome();   // refuses the installed app's data folder

        var workerDir = Environment.GetEnvironmentVariable("BR_REAL_WORKER_DIR");
        if (string.IsNullOrWhiteSpace(workerDir) || !Directory.Exists(workerDir))
        {
            output.WriteLine("skipped: BR_REAL_WORKER_DIR not set");
            return;
        }

        var root = TestEnv.NewDir("e2e-exe");   // never next to BR_REAL_HOME: the old code deleted <parent>\e2e-exe

        var install = Path.Combine(root, "install");
        CopyTree(workerDir, Path.Combine(install, "worker"));
        CopyTree(BuildDir("FakeAgent"), install);
        File.Move(Path.Combine(install, "FakeAgent.exe"), Path.Combine(install, InstallLayout.AgentExe));

        var home = Path.Combine(root, "home");
        var device = Environment.GetEnvironmentVariable("BR_REAL_DEVICE");
        if (string.IsNullOrWhiteSpace(device)) device = "auto";
        TestEnv.Settings(home, s =>
        {
            s.Inference.Fake = false;
            s.Inference.ModelDir = models;
            s.Inference.ImageTokens = 140;
            s.Inference.Device = device;
            s.Index.Dim = 768;
            s.Security.MemoryIsolation = true;
        });
        if (File.Exists(DataPaths.DeviceCache(home)))
        {
            File.Delete(DataPaths.DeviceCache(home));
        }

        var workerExe = InstallLayout.Full(install, InstallLayout.WorkerExe);
        var outFile = Path.Combine(root, "out.txt");
        var agentArgs = new List<string> { workerExe, home, outFile, "--real" };
        var wav = Environment.GetEnvironmentVariable("BR_REAL_WAV");
        if (!string.IsNullOrWhiteSpace(wav))
        {
            agentArgs.Add("--wav");
            agentArgs.Add(wav);
        }

        var psi = new ProcessStartInfo(Path.Combine(install, InstallLayout.AgentExe), agentArgs) { UseShellExecute = false };
        using var agent = Process.Start(psi)!;
        Assert.True(agent.WaitForExit((int)TimeSpan.FromMinutes(30).TotalMilliseconds), "fake agent timed out");

        var lines = File.ReadAllLines(outFile);
        foreach (var l in lines) output.WriteLine(l);
        Assert.NotEmpty(lines);
        Assert.Equal("exit=0", lines[^1]);
        Assert.DoesNotContain(lines, l => l.StartsWith("agent error", StringComparison.OrdinalIgnoreCase));
        var helloLine = lines.FirstOrDefault(l => l.Contains("\"missing\":", StringComparison.OrdinalIgnoreCase)) ?? lines[0];
        Assert.Contains("\"missing\":[]", helloLine);
    }
}

/// <summary>A fact that is reported as skipped (not passed) unless each named environment variable points at an
/// existing folder.</summary>
public sealed class RequiresEnvFactAttribute : FactAttribute
{
    public RequiresEnvFactAttribute(params string[] variables)
    {
        foreach (var v in variables)
        {
            var value = Environment.GetEnvironmentVariable(v);
            if (string.IsNullOrWhiteSpace(value) || !Directory.Exists(value))
            {
                Skip = $"{v} is not set to an existing folder";
                return;
            }
        }
    }
}
