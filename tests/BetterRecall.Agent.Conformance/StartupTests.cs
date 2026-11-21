// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Text.Json;
using BetterRecall.Agent.Conformance.Harness;
using BetterRecall.Contracts;
using BetterRecall.Core.Ipc;

namespace BetterRecall.Agent.Conformance;

[Collection("AgentConformance")]
public sealed class StartupTests(InstallFixture install)
{
    [Fact]
    public async Task Starts_writes_pid_and_ui_pipe_accepts_ping_and_version()
    {
        await using var h = new AgentHarness(install);
        var (agent, serverPid) = await h.StartAndConnectUiAsync();

        Assert.Equal(agent.Id, serverPid);
        var pidFile = DataPaths.AgentPidFile(h.Home);
        Assert.True(File.Exists(pidFile), "agent.pid was not created.");
        Assert.Equal(agent.Id, int.Parse(File.ReadAllText(pidFile).Trim()));

        var (okPing, pingData, pingErr) = await h.Ui.RequestAsync("ui", "ping");
        Assert.True(okPing, pingErr);
        Assert.True(pingData.GetProperty("pong").GetBoolean());

        var (okVer, verData, verErr) = await h.Ui.RequestAsync("ui", "version");
        Assert.True(okVer, verErr);
        var version = verData.Deserialize(ContractsJson.Default.VersionReply)!;
        Assert.Equal(1, version.Protocol);
        Assert.Matches(@"^\d+\.\d+\.\d+", version.App);
    }

    [Fact]
    public async Task Second_agent_on_same_home_exits_and_keeps_first_running()
    {
        await using var h = new AgentHarness(install);
        var (first, _) = await h.StartAndConnectUiAsync();

        var second = h.StartAgent(); // the harness owns and disposes it
        using var cts = new CancellationTokenSource(10_000);
        await second.WaitForExitAsync(cts.Token);
        Assert.Equal(0, second.ExitCode);
        Assert.False(first.HasExited);

        Assert.Equal(first.Id, int.Parse(File.ReadAllText(DataPaths.AgentPidFile(h.Home)).Trim()));
        var (ok, d, err) = await h.Ui.RequestAsync("ui", "ping");
        Assert.True(ok, err);
        Assert.True(d.GetProperty("pong").GetBoolean());
    }

    [Fact]
    public async Task Two_agents_on_different_homes_run_side_by_side()
    {
        await using var h = new AgentHarness(install);
        var home2 = h.NewExtraHome();

        var (agent1, pid1) = await h.StartAndConnectUiAsync("ui1", h.Home);
        var (agent2, pid2) = await h.StartAndConnectUiAsync("ui2", home2);

        Assert.NotEqual(agent1.Id, agent2.Id);
        Assert.Equal(agent1.Id, pid1);
        Assert.Equal(agent2.Id, pid2);
        Assert.Equal(agent1.Id, int.Parse(File.ReadAllText(DataPaths.AgentPidFile(h.Home)).Trim()));
        Assert.Equal(agent2.Id, int.Parse(File.ReadAllText(DataPaths.AgentPidFile(home2)).Trim()));

        var (ok1, d1, err1) = await h.Ui.RequestAsync("ui1", "ping");
        var (ok2, d2, err2) = await h.Ui.RequestAsync("ui2", "ping");
        Assert.True(ok1, err1);
        Assert.True(ok2, err2);
        Assert.True(d1.GetProperty("pong").GetBoolean());
        Assert.True(d2.GetProperty("pong").GetBoolean());
    }

    [Fact]
    public async Task Untrusted_client_is_rejected_on_ui_and_mcp_pipes()
    {
        await using var h = new AgentHarness(install, s => s.Mcp.Enabled = true);
        await h.StartAndConnectUiAsync();

        var sid = PipeNames.UserSid();
        var (uiRes, uiClient) = await PipeClient.ConnectAsync(PipeNames.Ui(sid, h.Home), install.AgentExe, 3000);
        Assert.Equal(PipeConnectResult.Ok, uiRes);
        await using (uiClient!)
        {
            var line = await uiClient.ReadLineAsync();
            Assert.Equal("{\"ok\":false,\"error\":\"Client not allowed.\"}", line);
            Assert.Null(await uiClient.ReadLineAsync());
        }

        var (mcpRes, mcpClient) = await PipeClient.ConnectAsync(PipeNames.Mcp(sid, h.Home), install.AgentExe, 3000);
        Assert.Equal(PipeConnectResult.Ok, mcpRes);
        await using (mcpClient!)
        {
            var line = await mcpClient.ReadLineAsync();
            Assert.Equal("{\"ok\":false,\"error\":\"Client not allowed.\"}", line);
            Assert.Null(await mcpClient.ReadLineAsync());
        }
    }
}
