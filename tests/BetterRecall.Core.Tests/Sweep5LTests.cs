// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Diagnostics;
using System.IO.Pipes;
using System.Text;
using System.Text.Json;
using BetterRecall.Contracts;
using BetterRecall.Core.Crypto;
using BetterRecall.Core.Ipc;
using StepClock = BetterRecall.Core.Ipc.StepClock;

namespace BetterRecall.Core.Tests;

/// <summary>Sweep 5 batch L: launcher/bin-current (C2), MCP upgrade window (B7), StepClock (C3), SUBST/junction RealPath (B8).</summary>
public sealed class Sweep5LTests : IAsyncLifetime
{
    private static readonly string Self = Environment.ProcessPath!;
    private readonly TempStore _t = new();
    private readonly TempHome _home = new();
    private readonly string _tag = Guid.NewGuid().ToString("N")[..10];
    private readonly string _root = Path.Combine(TestEnv.Root, "s5l-" + Guid.NewGuid().ToString("N")[..10]);
    private readonly List<string> _junctions = [];
    private SettingsStore _settings = null!;
    private KeyVault _vault = null!;
    private CommandProcessor _processor = null!;
    private PipeManager _pipes = null!;

    private const string OldFolder = "0.2.3-dev-00b55ae338ad";
    private const string NewFolder = "0.2.3-dev-1df40c2e4158";
    private const string StatusCall = "{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/call\",\"params\":{\"name\":\"recall_status\"}}";

    private string PublicName => $"BRtest.{_tag}.{_settings.Current.Ipc.PipeName}";

    public async Task InitializeAsync()
    {
        Directory.CreateDirectory(_root);
        _settings = SettingsStore.Load(_home.Path);
        Set("{\"ipc\":{\"enabled\":true}}");
        _vault = new KeyVault(_home.Path, () => _settings.Current, probeHello: false);
        _processor = new CommandProcessor(new CommandDeps
        {
            Settings = _settings, Store = () => _t.Store, Keys = _vault, Agent = new FakeAgent(),
            Embedder = new FakeEmbedder(), Thumbnailer = new FakeThumbs(), PurgeAll = _ => Task.FromResult(0),
        });
        _pipes = new PipeManager(_settings, _processor, Path.GetDirectoryName(Self)!, () => _t.Store, (k, s) => k switch
        {
            PipeKind.Ui => $"BRtest.{_tag}.ui",
            PipeKind.Mcp => $"BRtest.{_tag}.mcp",
            _ => $"BRtest.{_tag}.{s.Ipc.PipeName}",
        });
        await _pipes.StartAsync();
    }

    public async Task DisposeAsync()
    {
        await _pipes.DisposeAsync();
        _vault.Dispose();
        _t.Dispose();
        _home.Dispose();
        foreach (var j in _junctions)
        {
            try { if (Directory.Exists(j)) Directory.Delete(j, recursive: false); }
            catch (IOException) { }
            catch (UnauthorizedAccessException) { }
        }
        try { Directory.Delete(_root, true); } catch (IOException) { } catch (UnauthorizedAccessException) { }
    }

    private void Set(string json) => Assert.Null(_settings.TryUpdate(JsonDocument.Parse(json).RootElement));

    private void Layout(string currentFolder)
    {
        foreach (var f in new[] { OldFolder, NewFolder })
        {
            Directory.CreateDirectory(Path.Combine(_root, f));
            File.WriteAllBytes(Path.Combine(_root, f, InstallLayout.AgentExe), []);
        }
        SetCurrent(currentFolder);
    }

    private void SetCurrent(string folder)
    {
        var bin = Path.Combine(_root, InstallLayout.BinDir);
        Directory.CreateDirectory(bin);
        File.WriteAllText(Path.Combine(bin, InstallLayout.CurrentFile), Path.Combine(_root, folder));
    }

    private string CreateJunction(string name, string target)
    {
        var link = Path.Combine(_root, name);
        var psi = new ProcessStartInfo("cmd.exe")
        {
            UseShellExecute = false,
            CreateNoWindow = true,
        };
        psi.ArgumentList.Add("/c");
        psi.ArgumentList.Add("mklink");
        psi.ArgumentList.Add("/J");
        psi.ArgumentList.Add(link);
        psi.ArgumentList.Add(target);
        using var p = Process.Start(psi)!;
        p.WaitForExit(5000);
        Assert.Equal(0, p.ExitCode);
        _junctions.Add(link);
        return link;
    }

    // ---------------------------------------------------------------- C2: bin\current shared delete + replace

    [Fact]
    public void C2_CurrentInstall_shares_delete_so_replace_succeeds_during_concurrent_read()
    {
        Layout(OldFolder);
        var currentFile = Path.Combine(_root, InstallLayout.BinDir, InstallLayout.CurrentFile);
        var tmp = currentFile + ".tmp";

        // Reader matching InstallLayout.CurrentInstall's FileShare.ReadWrite | FileShare.Delete does not block File.Replace.
        using (var reader = new FileStream(currentFile, FileMode.Open, FileAccess.Read, FileShare.ReadWrite | FileShare.Delete))
        {
            Assert.Equal(Path.Combine(_root, OldFolder), InstallLayout.CurrentInstall(_root));
            File.WriteAllText(tmp, Path.Combine(_root, NewFolder));
            File.Replace(tmp, currentFile, destinationBackupFileName: null, ignoreMetadataErrors: true);
        }
        Assert.Equal(Path.Combine(_root, NewFolder), InstallLayout.CurrentInstall(_root));

        // An external reader that holds FileShare.Read only does not block CurrentInstall either.
        using (var external = new FileStream(currentFile, FileMode.Open, FileAccess.Read, FileShare.Read))
        {
            Assert.Equal(Path.Combine(_root, NewFolder), InstallLayout.CurrentInstall(_root));
        }
    }

    // ---------------------------------------------------------------- C3: StepClock multi-step and sub-tolerance steps

    [Fact]
    public void C3_StepClock_preserves_until_across_multiple_backward_steps_and_sub_tolerance_steps()
    {
        var steps = new List<TimeSpan>();
        var clock = new StepClock(steps.Add);
        var boot = new DateTime(2026, 11, 18, 12, 0, 0, DateTimeKind.Utc);
        long ms = 60_000;
        var wall = boot.AddMilliseconds(ms);   // 12:01
        Assert.Equal(wall, clock.Bound(wall, ms, boot));

        // Step 1: -60 min at 12:05 -> wall 11:05, _until = 12:05.
        ms += 240_000;
        wall = new DateTime(2026, 11, 18, 11, 5, 0, DateTimeKind.Utc);
        Assert.Equal(new DateTime(2026, 11, 18, 12, 5, 0, DateTimeKind.Utc), clock.Bound(wall, ms, boot));

        // Step 2: 5 min later, -2 min -> wall 11:08, _until must stay 12:05 (not shrink to 11:10).
        ms += 300_000;
        wall = new DateTime(2026, 11, 18, 11, 8, 0, DateTimeKind.Utc);
        Assert.Equal(new DateTime(2026, 11, 18, 12, 5, 0, DateTimeKind.Utc), clock.Bound(wall, ms, boot));
        Assert.Equal(2, steps.Count);

        // Cumulative sub-10 s backward steps (-6 s x 3) must not silently advance _gap.
        var subSteps = new List<TimeSpan>();
        var subClock = new StepClock(subSteps.Add);
        var subWall = new DateTime(2026, 11, 18, 12, 1, 0, DateTimeKind.Utc);
        long subMs = 60_000;
        Assert.Equal(subWall, subClock.Bound(subWall, subMs, boot));
        Assert.Equal(subWall.AddSeconds(-6), subClock.Bound(subWall.AddSeconds(-6), subMs, boot));
        Assert.Empty(subSteps);
        Assert.Equal(subWall, subClock.Bound(subWall.AddSeconds(-12), subMs, boot));
        Assert.Single(subSteps);
        Assert.Equal(subWall, subClock.Bound(subWall.AddSeconds(-18), subMs, boot));

        // Running StepClock across +2 h forward then -1 h backward catches the -1 h step.
        var fwdClock = new StepClock();
        var fwdBoot = new DateTime(2026, 11, 18, 10, 0, 0, DateTimeKind.Utc);
        Assert.Equal(new DateTime(2026, 11, 18, 12, 1, 0, DateTimeKind.Utc),
            fwdClock.Bound(new DateTime(2026, 11, 18, 12, 1, 0, DateTimeKind.Utc), 60_000, fwdBoot));
        Assert.Equal(new DateTime(2026, 11, 18, 12, 5, 0, DateTimeKind.Utc),
            fwdClock.Bound(new DateTime(2026, 11, 18, 11, 5, 0, DateTimeKind.Utc), 300_000, fwdBoot));
    }

    [Fact]
    public async Task C3_PipeClient_and_PipeServer_stay_trusted_across_two_backward_clock_steps()
    {
        var server = new PipeServer("BRtest.c3." + _tag, PipeKind.Public, _ => true, (_, _) => Task.CompletedTask);
        var self = ProcessInfo.Read(Environment.ProcessId, ProcessInfo.CurrentSession);
        var boot = new DateTime(2026, 11, 18, 12, 0, 0, DateTimeKind.Utc);
        var srvClock = new StepClock();
        long ms = 60_000;
        var started = new DateTime(2026, 11, 18, 12, 1, 0, DateTimeKind.Utc);
        Assert.Equal(started, srvClock.Bound(started, ms, boot));

        ms += 240_000;
        var bound1 = srvClock.Bound(new DateTime(2026, 11, 18, 11, 5, 0, DateTimeKind.Utc), ms, boot);
        ms += 300_000;
        var bound2 = srvClock.Bound(new DateTime(2026, 11, 18, 11, 8, 0, DateTimeKind.Utc), ms, boot);
        var peer = self with { CreatedUtc = new DateTime(2026, 11, 18, 12, 4, 0, DateTimeKind.Utc) };
        Assert.Null(server.Check(peer, bound1));
        Assert.Null(server.Check(peer, bound2));

        // Real pipe seam: two consecutive backward steps (-60 min, then -2 min) keep PipeClient trusted.
        var (r1, c1) = await PipeClient.ConnectAsync(PublicName, Self, 2000, () => DateTime.UtcNow.AddMinutes(-60), default);
        Assert.Equal(PipeConnectResult.Ok, r1);
        await using (c1!) Assert.True(Envelope.Parse((await c1!.RequestAsync("ping"))!).Ok);

        var (r2, c2) = await PipeClient.ConnectAsync(PublicName, Self, 2000, () => DateTime.UtcNow.AddMinutes(-62), default);
        Assert.Equal(PipeConnectResult.Ok, r2);
        await using (c2!) Assert.True(Envelope.Parse((await c2!.RequestAsync("ping"))!).Ok);

        var (r3, _) = await PipeClient.ConnectAsync(PublicName, @"C:\elsewhere\BetterRecall.Agent.exe", 2000, () => DateTime.UtcNow.AddMinutes(-62), default);
        Assert.Equal(PipeConnectResult.NotTrusted, r3);
    }

    // ---------------------------------------------------------------- B8: RealPath / junction trust

    [Fact]
    public async Task B8_RealPath_SamePath_SameInstallFile_and_Pipe_trust_resolve_junctions()
    {
        var realApp = Path.Combine(_root, "b8-real");
        var verReal = Path.Combine(realApp, "0.2.4-bbbbbbbbbbbb");
        Directory.CreateDirectory(verReal);
        Directory.CreateDirectory(Path.Combine(realApp, InstallLayout.BinDir));
        var juncApp = CreateJunction("b8-junc", realApp);
        var selfJunc = CreateJunction("self-junc", Path.GetDirectoryName(Self)!);

        var expectedReal = InstallLayout.RealPath(realApp);
        Assert.Equal(expectedReal, InstallLayout.RealPath(juncApp));
        Assert.Equal(
            Path.Combine(expectedReal, "0.2.4-bbbbbbbbbbbb", "nonexistent.exe"),
            InstallLayout.RealPath(Path.Combine(juncApp, "0.2.4-bbbbbbbbbbbb", "nonexistent.exe")));

        var selfViaJunc = Path.Combine(selfJunc, Path.GetFileName(Self));
        Assert.True(ProcessInfo.SamePath(selfViaJunc, Self));
        Assert.True(PipeManager.SameInstallFile(
            verReal,
            Path.Combine(verReal, InstallLayout.CliExe),
            Path.Combine(juncApp, InstallLayout.BinDir, InstallLayout.CliShim)));
        Assert.True(PipeManager.SameInstallFile(
            Path.Combine(juncApp, "0.2.4-bbbbbbbbbbbb"),
            Path.Combine(verReal, InstallLayout.CliExe),
            Path.Combine(juncApp, OldFolder, InstallLayout.CliExe)));

        // Real pipe client check expecting the agent exe through a junction matches QueryFullProcessImageName's real path.
        var (r, c) = await PipeClient.ConnectAsync(PublicName, selfViaJunc, 2000);
        Assert.Equal(PipeConnectResult.Ok, r);
        await using (c!) Assert.True(Envelope.Parse((await c!.RequestAsync("ping"))!).Ok);

        // Real pipe server allowClients entry through a junction admits the running client image.
        Set("{\"ipc\":{\"allowClients\":[" + JsonSerializer.Serialize(selfViaJunc) + "]}}");
        (r, c) = await PipeClient.ConnectAsync(PublicName, Self, 2000);
        Assert.Equal(PipeConnectResult.Ok, r);
        await using (c!) Assert.True(Envelope.Parse((await c!.RequestAsync("ping"))!).Ok);
    }
}
