// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using BetterRecall.Agent.Host;
using BetterRecall.Contracts;

namespace BetterRecall.Agent.Tests;

/// <summary>A dev or test agent on another data folder must not share the installed agent's names or registry entries.</summary>
public class IsolationTests
{
    private static readonly string RealHome =
        Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.LocalApplicationData), "BetterRecall");

    [Fact]
    public void Installed_folder_keeps_the_plain_names_others_get_their_own()
    {
        Assert.True(DataPaths.IsDefaultHome(RealHome + "\\"));
        Assert.Equal(PipeNames.AgentMutex, PipeNames.AgentMutexFor(RealHome));
        Assert.Equal(PipeNames.AgentQuitEvent, PipeNames.AgentQuitEventFor(RealHome));
        var dev = @"K:\scratch\betterrecall-dev-home";
        Assert.False(DataPaths.IsDefaultHome(dev));
        Assert.StartsWith(PipeNames.AgentMutex + ".", PipeNames.AgentMutexFor(dev));
        Assert.NotEqual(PipeNames.AgentMutexFor(dev), PipeNames.AgentMutexFor(@"K:\scratch\other"));
        Assert.Equal(PipeNames.AgentQuitEventFor(dev), PipeNames.AgentQuitEventFor(dev.ToLowerInvariant() + "\\"));
    }

    [Fact]
    public void Only_the_installed_layout_registers_autostart()
    {
        Assert.False(StartupRegistration.IsInstalled(@"E:\BetterRecall\artifacts\dev", RealHome));
        Assert.False(StartupRegistration.IsInstalled(Path.Combine(RealHome, "app", "0.3.0-abc"), @"K:\scratch\h"));
        if (string.IsNullOrEmpty(Environment.GetEnvironmentVariable(InstallLayout.LauncherEnv)))
            Assert.True(StartupRegistration.IsInstalled(Path.Combine(RealHome, "app", "0.3.0-abc"), RealHome));
    }
}
