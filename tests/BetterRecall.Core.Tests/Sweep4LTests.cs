// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.IO.Pipes;
using System.Text;
using System.Text.Json;
using BetterRecall.Contracts;
using BetterRecall.Core.Crypto;
using BetterRecall.Core.Ipc;
using BetterRecall.Mcp;
using BetterRecall.Stub;

namespace BetterRecall.Core.Tests;

/// <summary>Sweep 4 batch L: launcher, MCP and process trust (B5 + C4, T2, B12). Seams run over real pipes: the real
/// PipeManager and CommandProcessor play the agent, this test process is its image.</summary>
public sealed class Sweep4LTests : IAsyncLifetime
{
    private static readonly string Self = Environment.ProcessPath!;
    private readonly TempStore _t = new();
    private readonly TempHome _home = new();
    private readonly string _tag = Guid.NewGuid().ToString("N")[..10];
    private readonly string _root = Path.Combine(TestEnv.Root, "s4l-" + Guid.NewGuid().ToString("N")[..10]);
    private SettingsStore _settings = null!;
    private KeyVault _vault = null!;
    private PipeManager _pipes = null!;

    private string PublicName => $"BRtest.{_tag}.{_settings.Current.Ipc.PipeName}";

    public async Task InitializeAsync()
    {
        Directory.CreateDirectory(_root);
        _settings = SettingsStore.Load(_home.Path);
        Set("{\"ipc\":{\"enabled\":true}}");
        _vault = new KeyVault(_home.Path, () => _settings.Current, probeHello: false);
        var processor = new CommandProcessor(new CommandDeps
        {
            Settings = _settings, Store = () => _t.Store, Keys = _vault, Agent = new FakeAgent(),
            Embedder = new FakeEmbedder(), Thumbnailer = new FakeThumbs(), PurgeAll = _ => Task.FromResult(0),
        });
        // The agent's install dir is this test process's folder, so its image is "our own file" for B12.
        _pipes = new PipeManager(_settings, processor, Path.GetDirectoryName(Self)!, () => _t.Store, (k, s) => k switch
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
        try { Directory.Delete(_root, true); } catch (IOException) { } catch (UnauthorizedAccessException) { }
    }

    private void Set(string json) => Assert.Null(_settings.TryUpdate(JsonDocument.Parse(json).RootElement));

    // ---------------------------------------------------------------- T2: backward clock step

    [Fact]
    public void T2_StartBound_follows_the_boot_clock_only_after_a_step_back()
    {
        var wall = new DateTime(2026, 11, 16, 11, 51, 52, DateTimeKind.Utc);
        long up = 3_600_000;   // an hour since boot
        // No step: boot stamp + uptime == wall (+ the few seconds the System process starts after boot): wall stays.
        Assert.Equal(wall, ProcessInfo.StartBound(wall, up, wall.AddMilliseconds(-up).AddSeconds(3)));
        // Clock stepped back 2 h after boot: boot stamp + uptime is 2 h ahead of the wall clock and wins.
        var boot = wall.AddMilliseconds(-up).AddHours(2);
        Assert.Equal(boot.AddMilliseconds(up), ProcessInfo.StartBound(wall, up, boot));
        // Stepped forward: the wall clock is ahead, stays the bound. Unknown boot: wall clock.
        Assert.Equal(wall, ProcessInfo.StartBound(wall, up, wall.AddMilliseconds(-up).AddHours(-2)));
        Assert.Equal(wall, ProcessInfo.StartBound(wall, up, null));
        // The real one never falls behind the wall clock.
        var now = DateTime.UtcNow;
        Assert.True(ProcessInfo.StartBound(now) >= now);
    }

    [Fact]
    public async Task T2_Client_still_trusts_the_running_agent_after_the_clock_steps_back_2h()
    {
        var stepped = () => DateTime.UtcNow.AddHours(-2);
        // The old check: the agent (this process) "started after" the stepped connect time.
        Assert.Null(ProcessInfo.ImagePathIfStartedBefore(Environment.ProcessId, stepped()));
        // Real pipe, real agent-side checks, real process start times; only the client's wall clock is stepped.
        var (r, c) = await PipeClient.ConnectAsync(PublicName, Self, 2000, stepped, default);
        Assert.Equal(PipeConnectResult.Ok, r);
        await using (c!)
            Assert.True(Envelope.Parse((await c!.RequestAsync("ping"))!).Ok);
        // The image check still holds.
        (r, c) = await PipeClient.ConnectAsync(PublicName, @"C:\elsewhere\BetterRecall.Agent.exe", 2000, stepped, default);
        Assert.Equal(PipeConnectResult.NotTrusted, r);
    }

    [Fact]
    public void T2_Server_check_admits_a_client_started_before_a_step_back_and_still_refuses_pid_reuse()
    {
        var server = new PipeServer("BRtest.unused." + Guid.NewGuid().ToString("N"), PipeKind.Public, _ => true, (_, _) => Task.CompletedTask);
        var self = ProcessInfo.Read(Environment.ProcessId, ProcessInfo.CurrentSession);
        var wall = DateTime.UtcNow;
        long up = Environment.TickCount64;
        // The UI started before NTP stepped the clock back 2 h: its start shows 2 h "ahead"; so does the boot stamp.
        var steppedBoot = wall.AddMilliseconds(-up).AddHours(2);
        var ui = self with { CreatedUtc = self.CreatedUtc!.Value.AddHours(2) };
        Assert.Equal("pid reuse", server.Check(ui, wall));   // before T2
        Assert.Null(server.Check(ui, ProcessInfo.StartBound(wall, up, steppedBoot)));
        // Without a step a process newer than the connection is still a reused pid.
        var unsteppedBoot = wall.AddMilliseconds(-up);
        Assert.Equal("pid reuse", server.Check(self with { CreatedUtc = wall.AddSeconds(1) }, ProcessInfo.StartBound(wall, up, unsteppedBoot)));
    }

    // ---------------------------------------------------------------- B12: allow list across version folders

    [Theory]
    [InlineData(@"C:\d\app\0.2.4-bbbbbbbbbbbb", @"C:\d\app\0.2.4-bbbbbbbbbbbb\betterrecall.exe", @"C:\d\app\0.2.3-dev-00b55ae338ad\betterrecall.exe", true)]
    [InlineData(@"C:\d\app\0.2.4-bbbbbbbbbbbb", @"C:\d\app\0.2.4-bbbbbbbbbbbb\betterrecall.exe", @"C:\d\app\bin\betterrecall.cmd", true)]
    [InlineData(@"C:\d\app\0.2.4-bbbbbbbbbbbb", @"C:\d\app\0.2.4-bbbbbbbbbbbb\ui\BetterRecall.UI.exe", @"C:\d\app\0.2.3-aaaaaaaaaaaa\ui\BetterRecall.UI.exe", true)]
    [InlineData(@"C:\d\app\0.2.4-bbbbbbbbbbbb", @"C:\d\app\0.2.4-bbbbbbbbbbbb\betterrecall-mcp.exe", @"C:\d\app\0.2.3-aaaaaaaaaaaa\betterrecall.exe", false)]   // another file
    [InlineData(@"C:\d\app\0.2.4-bbbbbbbbbbbb", @"C:\d\app\0.2.4-bbbbbbbbbbbb\betterrecall-mcp.exe", @"C:\d\app\bin\betterrecall.cmd", false)]
    [InlineData(@"C:\d\app\0.2.4-bbbbbbbbbbbb", @"C:\d\app\0.2.3-aaaaaaaaaaaa\betterrecall.exe", @"C:\d\app\0.2.2-cccccccccccc\betterrecall.exe", false)] // client not in the agent's install
    [InlineData(@"C:\d\app\0.2.4-bbbbbbbbbbbb", @"C:\d\app\0.2.4-bbbbbbbbbbbb\betterrecall.exe", @"C:\d\app\tools\betterrecall.exe", false)]       // not a version folder
    [InlineData(@"C:\d\app\0.2.4-bbbbbbbbbbbb", @"C:\d\app\0.2.4-bbbbbbbbbbbb\betterrecall.exe", @"C:\other\app\0.2.3-aaaaaaaaaaaa\betterrecall.exe", false)] // another root
    [InlineData(@"C:\d\app\0.2.4-bbbbbbbbbbbb", @"C:\Tools\my.exe", @"C:\d\app\0.2.3-aaaaaaaaaaaa\betterrecall.exe", false)]
    public void B12_SameInstallFile(string install, string image, string entry, bool allowed)
        => Assert.Equal(allowed, PipeManager.SameInstallFile(install, image, entry));

    [Fact]
    public async Task B12_Allow_list_entry_of_an_older_version_folder_admits_the_same_file_of_this_install()
    {
        // The agent runs from this test's folder; the entry names the same exe in a sibling version folder, as an
        // entry written before an upgrade does.
        var install = Path.GetDirectoryName(Self)!;
        var parent = Path.GetDirectoryName(install)!;
        var old = Path.Combine(parent, "0.2.3-dev-00b55ae338ad", Path.GetFileName(Self));
        Set("{\"ipc\":{\"allowClients\":[" + JsonSerializer.Serialize(old) + "]}}");
        var (r, c) = await PipeClient.ConnectAsync(PublicName, Self, 2000);
        Assert.Equal(PipeConnectResult.Ok, r);
        await using (c!)
            Assert.True(Envelope.Parse((await c!.RequestAsync("ping"))!).Ok);

        // Another file name in that folder, or the same name in a folder that is not a version folder: refused.
        foreach (var entry in new[] { Path.Combine(parent, "0.2.3-dev-00b55ae338ad", "other.exe"), Path.Combine(parent, "tools", Path.GetFileName(Self)) })
        {
            Set("{\"ipc\":{\"allowClients\":[" + JsonSerializer.Serialize(entry) + "]}}");
            (r, c) = await PipeClient.ConnectAsync(PublicName, Self, 2000);
            Assert.Equal(PipeConnectResult.Ok, r);
            await using (c!)
                Assert.Equal("Client not allowed.", Envelope.Parse((await c!.ReadLineAsync())!).Error);
        }
    }

    // ---------------------------------------------------------------- B5 + C4: MCP across a launcher upgrade

    private const string OldFolder = "0.2.3-dev-00b55ae338ad";
    private const string NewFolder = "0.2.3-dev-1df40c2e4158";

    /// <summary>&lt;root&gt;\{old,new}\BetterRecall.Agent.exe and bin\current naming currentFolder.</summary>
    private string Layout(string currentFolder)
    {
        foreach (var f in new[] { OldFolder, NewFolder })
        {
            Directory.CreateDirectory(Path.Combine(_root, f));
            File.WriteAllBytes(Path.Combine(_root, f, InstallLayout.AgentExe), []);
        }
        SetCurrent(currentFolder);
        return _root;
    }

    private void SetCurrent(string folder)
    {
        Directory.CreateDirectory(Path.Combine(_root, InstallLayout.BinDir));
        File.WriteAllText(Path.Combine(_root, InstallLayout.BinDir, InstallLayout.CurrentFile), Path.Combine(_root, folder));
    }

    [Fact]
    public void B5_IsStale_only_when_bin_current_names_another_install()
    {
        Layout(NewFolder);
        Assert.True(McpPipe.IsStale(Path.Combine(_root, OldFolder)));
        Assert.False(McpPipe.IsStale(Path.Combine(_root, NewFolder)));
        Assert.False(McpPipe.IsStale(Path.GetDirectoryName(Self)!));   // dev layout: no version folder, never stale
        SetCurrent(OldFolder);
        Assert.False(McpPipe.IsStale(Path.Combine(_root, OldFolder)));
        Assert.Equal(Path.Combine(_root, OldFolder), InstallLayout.CurrentInstall(_root));
    }

    /// <summary>An MCP server of install folder "folder": it trusts the agent exe next to it, except that the new
    /// install's server trusts this process (it plays the agent of the new install).</summary>
    private McpPipe Pipe(string folder, bool relayed)
        => new(PublicName, folder == NewFolder ? Self : Path.Combine(_root, folder, InstallLayout.AgentExe), Path.Combine(_root, folder), relayed);

    private const string StatusCall = "{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/call\",\"params\":{\"name\":\"recall_status\"}}";

    [Fact]
    public async Task B5_Stale_server_without_relay_says_restart_the_client_and_exits()
    {
        Layout(NewFolder);
        var pipe = Pipe(OldFolder, relayed: false);
        var output = new StringWriter { NewLine = "\n" };
        int code = await McpHost.RunAsync(new StringReader(StatusCall + "\n" + StatusCall + "\n"), output, new McpServer(pipe.CallAsync), pipe);
        Assert.Equal(InstallLayout.McpRestartExitCode, code);
        var lines = output.ToString().Split('\n', StringSplitOptions.RemoveEmptyEntries);
        var r = Assert.Single(lines);   // exits after the first answer, freeing the old folder
        var result = JsonDocument.Parse(r).RootElement.GetProperty("result");
        Assert.True(result.GetProperty("isError").GetBoolean());
        Assert.Equal(McpPipe.Updated, result.GetProperty("content")[0].GetProperty("text").GetString());
    }

    [Fact]
    public async Task B5_Not_trusted_without_an_upgrade_keeps_the_old_answer()
    {
        Layout(OldFolder);   // bin\current still names this server's install
        var pipe = Pipe(OldFolder, relayed: true);
        var output = new StringWriter { NewLine = "\n" };
        Assert.Equal(0, await McpHost.RunAsync(new StringReader(StatusCall + "\n"), output, new McpServer(pipe.CallAsync), pipe));
        Assert.Contains(McpPipe.NotTrusted, output.ToString());
        Assert.False(pipe.Stale);
    }

    [Fact]
    public async Task B5_Relay_restarts_a_stale_server_from_the_new_install_and_the_same_call_succeeds()
    {
        Layout(NewFolder);   // the launcher already upgraded: the agent runs from the new folder
        var started = new List<string>();
        await using var client = new RelayClient();
        var relay = new McpRelay(client.RelayIn, client.RelayOut, () => InstallLayout.CurrentInstall(_root), install =>
        {
            var folder = Path.GetFileName(install);
            lock (started) started.Add(folder);
            return new HostChild(Pipe(folder, relayed: true));
        }, poll: TimeSpan.FromMinutes(5));   // no idle swap: the call itself must find the old server stale
        SetCurrent(OldFolder);   // the session started before the upgrade
        var run = relay.RunAsync();
        await client.SendAsync("{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{\"protocolVersion\":\"2025-06-18\"}}");
        Assert.Equal(1, (await client.ReadAsync()).GetProperty("id").GetInt32());
        SetCurrent(NewFolder);   // launcher upgrade

        await client.SendAsync(StatusCall);
        var answer = await client.ReadAsync();
        Assert.Equal(2, answer.GetProperty("id").GetInt32());
        var result = answer.GetProperty("result");
        Assert.False(result.GetProperty("isError").GetBoolean(), result.ToString());
        Assert.True(JsonDocument.Parse(result.GetProperty("content")[0].GetProperty("text").GetString()!).RootElement.TryGetProperty("capturing", out _));
        lock (started) Assert.Equal([OldFolder, NewFolder], started);
        Assert.Equal(1, relay.Restarts);

        client.EndInput();
        Assert.Equal(0, await run.WaitAsync(TimeSpan.FromSeconds(10)));
    }

    [Fact]
    public async Task B5_Idle_relay_moves_to_the_new_install_so_the_old_folder_is_free()
    {
        Layout(OldFolder);
        var children = new List<(string Folder, HostChild Child)>();
        await using var client = new RelayClient();
        // Both servers trust this process: the old session keeps working until the swap, the new one after it.
        var relay = new McpRelay(client.RelayIn, client.RelayOut, () => InstallLayout.CurrentInstall(_root), install =>
        {
            var folder = Path.GetFileName(install);
            var child = new HostChild(new McpPipe(PublicName, Self, install, relayed: true));
            lock (children) children.Add((folder, child));
            return child;
        }, poll: TimeSpan.FromMilliseconds(100));
        var run = relay.RunAsync();
        await client.SendAsync(StatusCall);
        Assert.False((await client.ReadAsync()).GetProperty("result").GetProperty("isError").GetBoolean());

        SetCurrent(NewFolder);
        HostChild first;
        lock (children) first = children[0].Child;
        Assert.Equal(0, await first.Exited.WaitAsync(TimeSpan.FromSeconds(10)));   // ended by the relay, not by the client
        await client.SendAsync(StatusCall.Replace("\"id\":2", "\"id\":3"));
        var answer = await client.ReadAsync();
        Assert.Equal(3, answer.GetProperty("id").GetInt32());
        Assert.False(answer.GetProperty("result").GetProperty("isError").GetBoolean());
        lock (children) Assert.Equal([OldFolder, NewFolder], children.Select(c => c.Folder));

        client.EndInput();
        Assert.Equal(0, await run.WaitAsync(TimeSpan.FromSeconds(10)));
    }

    [Fact]
    public async Task B5_Relay_answers_pending_calls_with_an_error_when_no_new_install_appears()
    {
        Layout(OldFolder);
        await using var client = new RelayClient();
        var relay = new McpRelay(client.RelayIn, client.RelayOut, () => InstallLayout.CurrentInstall(_root),
            _ => new FakeChild(InstallLayout.McpRestartExitCode), waitForCurrent: TimeSpan.FromMilliseconds(300));
        var run = relay.RunAsync();
        await client.SendAsync("{\"jsonrpc\":\"2.0\",\"id\":\"a\\u0062\",\"method\":\"tools/call\",\"params\":{\"name\":\"recall_status\"}}");
        var answer = await client.ReadAsync();
        Assert.Equal("ab", answer.GetProperty("id").GetString());
        Assert.Contains("restart your MCP client", answer.GetProperty("error").GetProperty("message").GetString());
        Assert.Equal(InstallLayout.McpRestartExitCode, await run.WaitAsync(TimeSpan.FromSeconds(10)));
    }

    [Fact]
    public async Task B5_Relay_passes_any_other_exit_code_through()
    {
        Layout(OldFolder);
        await using var client = new RelayClient();
        var relay = new McpRelay(client.RelayIn, client.RelayOut, () => InstallLayout.CurrentInstall(_root), _ => new FakeChild(3));
        var run = relay.RunAsync();
        await client.SendAsync(StatusCall);
        Assert.Equal(3, await run.WaitAsync(TimeSpan.FromSeconds(10)));
        Assert.Equal(0, relay.Restarts);
    }

    [Fact]
    public void B5_MessageId_tells_requests_from_responses()
    {
        static byte[] B(string s) => Encoding.UTF8.GetBytes(s + "\n");
        Assert.Equal(("n:7", "7"), McpRelay.MessageId(B("{\"jsonrpc\":\"2.0\",\"id\":7,\"method\":\"ping\"}"), request: true));
        Assert.Null(McpRelay.MessageId(B("{\"jsonrpc\":\"2.0\",\"id\":7,\"method\":\"ping\"}"), request: false));
        Assert.Equal(("s:7", "\"7\""), McpRelay.MessageId(B("{\"jsonrpc\":\"2.0\",\"id\":\"7\",\"result\":{}}"), request: false));
        Assert.Null(McpRelay.MessageId(B("{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}"), request: true));
        Assert.Null(McpRelay.MessageId(B("{nope"), request: true));
        Assert.Equal(("n:1", "1"), McpRelay.MessageId([0xEF, 0xBB, 0xBF, .. B("{\"id\":1,\"method\":\"x\"}")], request: true));
    }

    // ---------------------------------------------------------------- helpers

    /// <summary>The MCP client's side of the relay's stdio, over two in-process named pipes.</summary>
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

    /// <summary>server end in direction dir, connected client end the other way.</summary>
    internal static (NamedPipeServerStream Server, NamedPipeClientStream Client) PipePair(PipeDirection dir)
    {
        var name = "BRtest.relay." + Guid.NewGuid().ToString("N");
        var server = new NamedPipeServerStream(name, dir, 1, PipeTransmissionMode.Byte, PipeOptions.Asynchronous);
        var client = new NamedPipeClientStream(".", name, dir == PipeDirection.Out ? PipeDirection.In : PipeDirection.Out, PipeOptions.Asynchronous);
        var wait = server.WaitForConnectionAsync();
        client.Connect(5000);
        wait.GetAwaiter().GetResult();
        return (server, client);
    }

    /// <summary>A real MCP server (McpHost loop, McpServer, McpPipe on the real agent pipe) running in this process.</summary>
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

    /// <summary>A server that reads one line and exits with code, answering nothing.</summary>
    private sealed class FakeChild : IMcpChild
    {
        private readonly NamedPipeServerStream _in, _out;

        public FakeChild(int code)
        {
            (_in, var childIn) = PipePair(PipeDirection.Out);
            (_out, var childOut) = PipePair(PipeDirection.In);
            Exited = Task.Run(async () =>
            {
                using var reader = new StreamReader(childIn);
                await reader.ReadLineAsync();
                await childOut.DisposeAsync();
                return code;
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
}
