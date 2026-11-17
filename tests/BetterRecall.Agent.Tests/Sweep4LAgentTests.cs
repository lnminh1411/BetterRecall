// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using BetterRecall.Agent.Host;
using BetterRecall.Contracts;

namespace BetterRecall.Agent.Tests;

/// <summary>Bug sweep 4, batch L: C6, autostart names the stable stub, not the download or a versioned folder.</summary>
public sealed class Sweep4LAgentTests : IDisposable
{
    private readonly string _root = Path.Combine(
        Environment.GetEnvironmentVariable("BR_TEST_TMP") is { Length: > 0 } t ? t : Path.GetTempPath(), "br-s4l-" + Guid.NewGuid().ToString("N")[..10], "app");
    private readonly string _install;
    private readonly string _download;

    public Sweep4LAgentTests()
    {
        _install = Path.Combine(_root, "0.2.3-dev-1df40c2e4158");
        Directory.CreateDirectory(_install);
        File.WriteAllBytes(Path.Combine(_install, InstallLayout.AgentExe), []);
        _download = Path.Combine(Path.GetDirectoryName(_root)!, "Downloads", "BetterRecall-0.2.3-dev-win-x64.exe");
        Directory.CreateDirectory(Path.GetDirectoryName(_download)!);
        File.WriteAllBytes(_download, []);
    }

    public void Dispose()
    {
        try { Directory.Delete(Path.GetDirectoryName(_root)!, true); } catch (IOException) { } catch (UnauthorizedAccessException) { }
    }

    private string Stub => Path.Combine(_root, InstallLayout.BinDir, InstallLayout.AutostartExe);

    private void WriteBin()
    {
        Directory.CreateDirectory(Path.Combine(_root, InstallLayout.BinDir));
        File.WriteAllBytes(Stub, []);
        File.WriteAllText(Path.Combine(_root, InstallLayout.BinDir, InstallLayout.CurrentFile), _install);
    }

    [Fact]
    public void C6_Launcher_started_agent_registers_the_stable_stub_not_the_download()
    {
        WriteBin();
        Assert.Equal(Stub, StartupRegistration.AutostartExe(_install, _download));
    }

    [Fact]
    public void C6_Agent_started_from_the_UI_registers_the_stable_stub_not_its_versioned_exe()
    {
        WriteBin();
        Assert.Equal(Stub, StartupRegistration.AutostartExe(_install, null));
    }

    [Fact]
    public void C6_Without_a_stub_the_old_order_holds()
    {
        // Older launchers wrote no stub: the launcher path, else this agent exe.
        Assert.Equal(_download, StartupRegistration.AutostartExe(_install, _download));
        Assert.Equal(InstallLayout.Full(_install, InstallLayout.AgentExe), StartupRegistration.AutostartExe(_install, null));
        Assert.Equal(InstallLayout.Full(_install, InstallLayout.AgentExe), StartupRegistration.AutostartExe(_install, _download + ".gone"));
        // A stub without bin\current is not a launcher root.
        Directory.CreateDirectory(Path.GetDirectoryName(Stub)!);
        File.WriteAllBytes(Stub, []);
        Assert.Equal(_download, StartupRegistration.AutostartExe(_install, _download));
    }
}
