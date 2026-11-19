// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Diagnostics;
using System.IO.Pipes;
using System.Text;
using System.Text.Json;
using BetterRecall.Contracts;
using BetterRecall.Core.Crypto;
using BetterRecall.Core.Ipc;
using BetterRecall.Mcp;
using BetterRecall.Stub;
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

    // ---------------------------------------------------------------- B7: MCP upgrade-window IOException / NotRunning

    [Fact]
    public async Task B7_McpPipe_IOException_mid_call_marks_stale_when_bin_current_moves()
    {
        Layout(OldFolder);
        var pipeName = "BRtest.b7io." + _tag;
        using var server = new NamedPipeServerStream(pipeName, PipeDirection.InOut, 1, PipeTransmissionMode.Byte, PipeOptions.Asynchronous);
        var srvTask = Task.Run(async () =>
        {
            await server.WaitForConnectionAsync();
            _ = await LineProtocol.ReadLineAsync(server, LineProtocol.MaxRequestBytes, default);
            SetCurrent(NewFolder);
            await server.DisposeAsync();
        });

        var pipe = new McpPipe(pipeName, Self, Path.Combine(_root, OldFolder), relayed: true);
        var output = new StringWriter { NewLine = "\n" };
        int code = await McpHost.RunAsync(new StringReader(StatusCall + "\n"), output, new McpServer(pipe.CallAsync), pipe);
        await srvTask;

        Assert.Equal(InstallLayout.McpRestartExitCode, code);
        Assert.True(pipe.Stale);
        Assert.Equal("", output.ToString());
    }

    [Fact]
    public async Task B7_McpPipe_IOException_waits_for_delayed_bin_current_flip()
    {
        Layout(OldFolder);
        var pipeName = "BRtest.b7grace." + _tag;
        using var server = new NamedPipeServerStream(pipeName, PipeDirection.InOut, 1, PipeTransmissionMode.Byte, PipeOptions.Asynchronous);
        var srvTask = Task.Run(async () =>
        {
            await server.WaitForConnectionAsync();
            _ = await LineProtocol.ReadLineAsync(server, LineProtocol.MaxRequestBytes, default);
            await server.DisposeAsync();
            await Task.Delay(150);
            SetCurrent(NewFolder);
        });

        var pipe = new McpPipe(pipeName, Self, Path.Combine(_root, OldFolder), relayed: true, staleGrace: TimeSpan.FromSeconds(2));
        var output = new StringWriter { NewLine = "\n" };
        int code = await McpHost.RunAsync(new StringReader(StatusCall + "\n"), output, new McpServer(pipe.CallAsync), pipe);
        await srvTask;

        Assert.Equal(InstallLayout.McpRestartExitCode, code);
        Assert.True(pipe.Stale);
    }

    [Fact]
    public async Task B7_McpPipe_IOException_without_upgrade_returns_no_response()
    {
        Layout(OldFolder);
        var pipeName = "BRtest.b7crash." + _tag;
        using var server = new NamedPipeServerStream(pipeName, PipeDirection.InOut, 1, PipeTransmissionMode.Byte, PipeOptions.Asynchronous);
        var srvTask = Task.Run(async () =>
        {
            await server.WaitForConnectionAsync();
            _ = await LineProtocol.ReadLineAsync(server, LineProtocol.MaxRequestBytes, default);
            await server.DisposeAsync();
        });

        var pipe = new McpPipe(pipeName, Self, Path.Combine(_root, OldFolder), relayed: true, staleGrace: TimeSpan.FromMilliseconds(150));
        var output = new StringWriter { NewLine = "\n" };
        int code = await McpHost.RunAsync(new StringReader(StatusCall + "\n"), output, new McpServer(pipe.CallAsync), pipe);
        await srvTask;

        Assert.Equal(0, code);
        Assert.False(pipe.Stale);
        Assert.Contains("No response from BetterRecall.", output.ToString());
    }

    [Fact]
    public async Task B7_McpPipe_NotRunning_marks_stale_when_bin_current_moved()
    {
        Layout(NewFolder);
        var pipe = new McpPipe("BRtest.b7nr." + _tag, Path.Combine(_root, OldFolder, InstallLayout.AgentExe), Path.Combine(_root, OldFolder), relayed: true);
        var output = new StringWriter { NewLine = "\n" };
        int code = await McpHost.RunAsync(new StringReader(StatusCall + "\n"), output, new McpServer(pipe.CallAsync), pipe);

        Assert.Equal(InstallLayout.McpRestartExitCode, code);
        Assert.True(pipe.Stale);
        Assert.Equal("", output.ToString());

        SetCurrent(OldFolder);
        var same = new McpPipe("BRtest.b7nr2." + _tag, Path.Combine(_root, OldFolder, InstallLayout.AgentExe), Path.Combine(_root, OldFolder), relayed: true);
        var sameOut = new StringWriter { NewLine = "\n" };
        Assert.Equal(0, await McpHost.RunAsync(new StringReader(StatusCall + "\n"), sameOut, new McpServer(same.CallAsync), same));
        Assert.False(same.Stale);
        Assert.Contains("BetterRecall not running", sameOut.ToString());
    }

    [Fact]
    public async Task B7_Relay_recovers_in_flight_call_killed_by_upgrade()
    {
        Layout(OldFolder);
        var seamPipe = "BRtest.b7seam." + _tag;
        var v1Seen = new TaskCompletionSource(TaskCreationOptions.RunContinuationsAsynchronously);
        var v1Server = new NamedPipeServerStream(seamPipe, PipeDirection.InOut, 1, PipeTransmissionMode.Byte, PipeOptions.Asynchronous);
        PipeServer? v2Server = null;
        var upgradeTask = Task.Run(async () =>
        {
            await v1Server.WaitForConnectionAsync();
            _ = await LineProtocol.ReadLineAsync(v1Server, LineProtocol.MaxRequestBytes, default);
            v1Seen.TrySetResult();
            await v1Server.DisposeAsync();   // simulate v1 agent dying mid-call (IOException)
            v2Server = new PipeServer(seamPipe, PipeKind.Mcp, _ => true, _processor.ServeAsync);
            v2Server.Start();
            SetCurrent(NewFolder);
        });

        try
        {
            await using var client = new RelayClient();
            var relay = new McpRelay(client.RelayIn, client.RelayOut, () => InstallLayout.CurrentInstall(_root), install =>
            {
                var folder = Path.GetFileName(install);
                var pipe = new McpPipe(seamPipe, Self, Path.Combine(_root, folder), relayed: true, staleGrace: TimeSpan.FromSeconds(5));
                return new HostChild(pipe);
            }, poll: TimeSpan.FromMinutes(5));

            var run = relay.RunAsync();
            await client.SendAsync(StatusCall);
            await v1Seen.Task.WaitAsync(TimeSpan.FromSeconds(10));
            await upgradeTask.WaitAsync(TimeSpan.FromSeconds(10));

            var answer = await client.ReadAsync();
            Assert.Equal(2, answer.GetProperty("id").GetInt32());
            var result = answer.GetProperty("result");
            Assert.False(result.GetProperty("isError").GetBoolean(), result.ToString());
            Assert.True(JsonDocument.Parse(result.GetProperty("content")[0].GetProperty("text").GetString()!).RootElement.TryGetProperty("capturing", out _));
            Assert.Equal(1, relay.Restarts);

            client.EndInput();
            Assert.Equal(0, await run.WaitAsync(TimeSpan.FromSeconds(10)));
        }
        finally
        {
            await v1Server.DisposeAsync();
            if (v2Server is not null) await v2Server.DisposeAsync();
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

    // ---------------------------------------------------------------- local relay helpers

    private sealed class RelayClient : IAsyncDisposable
    {
        private readonly NamedPipeServerStream _toRelay, _fromRelay;
        private readonly StreamReader _reader;

        public Stream RelayIn { get; }
        public Stream RelayOut { get; }

        public RelayClient()
        {
            (_toRelay, var relayIn) = PipePair(PipeDirection.Out);
            (_fromRelay, var relayOut) = PipePair(PipeDirection.In);
            RelayIn = relayIn;
            RelayOut = relayOut;
            _reader = new StreamReader(_fromRelay, new UTF8Encoding(false));
        }

        public async Task SendAsync(string line)
        {
            await _toRelay.WriteAsync(Encoding.UTF8.GetBytes(line + "\n"));
            await _toRelay.FlushAsync();
        }

        public async Task<JsonElement> ReadAsync()
        {
            var line = await _reader.ReadLineAsync().WaitAsync(TimeSpan.FromSeconds(15));
            Assert.NotNull(line);
            return JsonDocument.Parse(line).RootElement.Clone();
        }

        public void EndInput() => _toRelay.Dispose();

        public async ValueTask DisposeAsync()
        {
            await _toRelay.DisposeAsync();
            await _fromRelay.DisposeAsync();
            await RelayIn.DisposeAsync();
            await RelayOut.DisposeAsync();
        }
    }

    private sealed class HostChild : IMcpChild
    {
        private readonly NamedPipeServerStream _in, _out;

        public HostChild(McpPipe pipe)
        {
            (_in, var childIn) = PipePair(PipeDirection.Out);
            (_out, var childOut) = PipePair(PipeDirection.In);
            Exited = Task.Run(async () =>
            {
                using var reader = new StreamReader(childIn, new UTF8Encoding(false));
                await using var writer = new StreamWriter(childOut, new UTF8Encoding(false)) { AutoFlush = true, NewLine = "\n" };
                return await McpHost.RunAsync(reader, writer, new McpServer(pipe.CallAsync), pipe);
            });
        }

        public Stream Input => _in;
        public Stream Output => _out;
        public Task<int> Exited { get; }

        public void Dispose()
        {
            _in.Dispose();
            _out.Dispose();
        }
    }

    private static (NamedPipeServerStream Server, NamedPipeClientStream Client) PipePair(PipeDirection dir)
    {
        var name = "BRtest.s5relay." + Guid.NewGuid().ToString("N");
        var server = new NamedPipeServerStream(name, dir, 1, PipeTransmissionMode.Byte, PipeOptions.Asynchronous);
        var client = new NamedPipeClientStream(".", name, dir == PipeDirection.Out ? PipeDirection.In : PipeDirection.Out, PipeOptions.Asynchronous);
        var wait = server.WaitForConnectionAsync();
        client.Connect(5000);
        wait.GetAwaiter().GetResult();
        return (server, client);
    }
}
