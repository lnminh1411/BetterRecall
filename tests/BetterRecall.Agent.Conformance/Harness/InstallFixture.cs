// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using BetterRecall.Contracts;

namespace BetterRecall.Agent.Conformance.Harness;

/// <summary>
/// Stages one shared install layout in TestEnv.Root for the conformance run:
/// BetterRecall.Agent.exe (BR_AGENT_DIR or artifacts\bin\BetterRecall.Agent\&lt;pivot&gt;),
/// worker\BetterRecall.Worker.exe (BR_WORKER_DIR or artifacts\bin\BetterRecall.Worker\&lt;pivot&gt;),
/// betterrecall.exe, betterrecall-mcp.exe, and ui\BetterRecall.UI.exe (headless UiDriver).
/// </summary>
public sealed class InstallFixture : IDisposable
{
    public string InstallDir { get; }
    public string AgentExe => InstallLayout.Full(InstallDir, InstallLayout.AgentExe);
    public string CliExe => InstallLayout.Full(InstallDir, InstallLayout.CliExe);
    public string McpExe => InstallLayout.Full(InstallDir, InstallLayout.McpExe);
    public string UiExe => InstallLayout.Full(InstallDir, InstallLayout.UiExe);
    public string WorkerExe => InstallLayout.Full(InstallDir, InstallLayout.WorkerExe);

    public InstallFixture()
    {
        InstallDir = TestEnv.NewDir("install");
        try
        {
            var agentSource = ResolveAgentDir();
            CopyTree(agentSource, InstallDir);
            if (!File.Exists(AgentExe))
                throw new FileNotFoundException($"Agent exe not found in '{agentSource}'.", AgentExe);

            var workerTarget = Path.Combine(InstallDir, "worker");
            if (!File.Exists(WorkerExe) || !string.IsNullOrWhiteSpace(Environment.GetEnvironmentVariable("BR_WORKER_DIR")))
                CopyTree(BuildDir("BetterRecall.Worker"), workerTarget);

            if (!File.Exists(CliExe))
                CopyTree(BuildDir("BetterRecall.Cli"), InstallDir);

            if (!File.Exists(McpExe))
                CopyTree(BuildDir("BetterRecall.Mcp"), InstallDir);

            var uiTarget = Path.Combine(InstallDir, "ui");
            if (Directory.Exists(uiTarget)) Directory.Delete(uiTarget, recursive: true);
            CopyTree(BuildDir("UiDriver"), uiTarget);
            if (!File.Exists(UiExe))
                throw new FileNotFoundException("Headless UiDriver exe not found after staging.", UiExe);
        }
        catch
        {
            TestEnv.TryDeleteTree(InstallDir);
            throw;
        }
    }

    public void Dispose() => TestEnv.TryDeleteTree(InstallDir);

    private static string ResolveAgentDir()
    {
        if (Environment.GetEnvironmentVariable("BR_AGENT_DIR") is { } dir && !string.IsNullOrWhiteSpace(dir))
            return Path.GetFullPath(dir);
        return BuildDir("BetterRecall.Agent");
    }

    private static string RepoRoot()
    {
        var d = new DirectoryInfo(AppContext.BaseDirectory);
        while (d is not null && !File.Exists(Path.Combine(d.FullName, "BetterRecall.slnx"))) d = d.Parent;
        return d?.FullName ?? throw new InvalidOperationException("Repository root (BetterRecall.slnx) not found.");
    }

    private static string BuildDir(string project)
    {
        if (project == "BetterRecall.Worker"
            && Environment.GetEnvironmentVariable("BR_WORKER_DIR") is { } workerDir
            && !string.IsNullOrWhiteSpace(workerDir))
            return Path.GetFullPath(workerDir);

        var pivot = new DirectoryInfo(AppContext.BaseDirectory.TrimEnd('\\')).Name;
        var baseDir = Path.Combine(RepoRoot(), "artifacts", "bin", project);
        var dir = Path.Combine(baseDir, pivot);
        if (!Directory.Exists(dir)) dir = Directory.GetDirectories(baseDir).First();
        return dir;
    }

    private static void CopyTree(string from, string to)
    {
        Directory.CreateDirectory(to);
        foreach (var f in Directory.GetFiles(from, "*", SearchOption.AllDirectories))
        {
            var dst = Path.Combine(to, Path.GetRelativePath(from, f));
            Directory.CreateDirectory(Path.GetDirectoryName(dst)!);
            File.Copy(f, dst, overwrite: true);
        }
    }
}

[CollectionDefinition("AgentConformance")]
public sealed class AgentConformanceCollection : ICollectionFixture<InstallFixture>;
