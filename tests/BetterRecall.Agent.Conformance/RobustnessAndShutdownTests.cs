// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.IO.Pipes;
using System.Security.Principal;
using System.Text;
using System.Text.Json;
using BetterRecall.Agent.Conformance.Harness;
using BetterRecall.Contracts;

namespace BetterRecall.Agent.Conformance;

[Collection("AgentConformance")]
public sealed class RobustnessAndShutdownTests(InstallFixture install)
{
    [Fact]
    public async Task Truncated_frame_drops_client_and_agent_keeps_serving()
    {
        await using var h = new AgentHarness(install, s => s.Ipc.Enabled = true);
        await h.StartAndConnectUiAsync();
        await h.SetupPassphraseAndWaitStoreAsync();

        var pubName = PipeNames.Base(PipeNames.Public("BetterRecall", PipeNames.UserSid(), h.Home));

        // Send an unterminated line and close immediately.
        await using (var truncated = new NamedPipeClientStream(".", pubName, PipeDirection.InOut, PipeOptions.Asynchronous, TokenImpersonationLevel.Anonymous))
        {
            await truncated.ConnectAsync(3000);
            await truncated.WriteAsync("ping"u8.ToArray());
            await truncated.FlushAsync();
        }

        // Keep an incomplete line open briefly while another client makes a full request.
        await using (var stalled = new NamedPipeClientStream(".", pubName, PipeDirection.InOut, PipeOptions.Asynchronous, TokenImpersonationLevel.Anonymous))
        {
            await stalled.ConnectAsync(3000);
            await stalled.WriteAsync("search "u8.ToArray());
            await stalled.FlushAsync();

            await using var healthy = await h.WaitForPublicConnectAsync();
            var (ok, d, err) = Envelope.Parse((await healthy.RequestAsync("ping"))!);
            Assert.True(ok, err);
            Assert.True(d.GetProperty("pong").GetBoolean());
        }

        // Also send a truncated frame on the UI pipe and confirm the UI pipe keeps serving.
        var (rawOk, _) = await h.Ui.RawOpenAsync("rawUi", h.Home);
        Assert.True(rawOk);
        await h.Ui.RawWriteAsync("rawUi", "pin"u8.ToArray());
        await h.Ui.CloseAsync("rawUi");

        var (uiOk, uiData, uiErr) = await h.Ui.RequestAsync("ui", "ping");
        Assert.True(uiOk, uiErr);
        Assert.True(uiData.GetProperty("pong").GetBoolean());
    }

    [Fact]
    public async Task Oversized_frame_returns_error_closes_connection_and_agent_keeps_serving()
    {
        await using var h = new AgentHarness(install, s => s.Ipc.Enabled = true);
        await h.StartAndConnectUiAsync();
        await h.SetupPassphraseAndWaitStoreAsync();

        // Public pipe limit: 4096 bytes.
        await using (var pub = await h.WaitForPublicConnectAsync())
        {
            var reply = await pub.RequestAsync("search " + new string('x', 5000));
            Assert.Equal("{\"ok\":false,\"error\":\"Line exceeds 4096 bytes.\"}", reply);
            Assert.Null(await pub.ReadLineAsync());
        }

        await using (var healthyPub = await h.WaitForPublicConnectAsync())
        {
            var (ok, d, err) = Envelope.Parse((await healthyPub.RequestAsync("ping"))!);
            Assert.True(ok, err);
            Assert.True(d.GetProperty("pong").GetBoolean());
        }

        // UI pipe limit: 65536 bytes.
        var (rawOk, _) = await h.Ui.RawOpenAsync("rawUi", h.Home);
        Assert.True(rawOk);
        await h.Ui.RawWriteAsync("rawUi", Encoding.UTF8.GetBytes("search " + new string('x', 70_000) + "\n"));
        var (eof1, errLine) = await h.Ui.RawReadLineAsync("rawUi");
        Assert.False(eof1);
        Assert.Equal("{\"ok\":false,\"error\":\"Line exceeds 65536 bytes.\"}", errLine);
        var (eof2, _) = await h.Ui.RawReadLineAsync("rawUi");
        Assert.True(eof2);
        await h.Ui.CloseAsync("rawUi");

        var (uiOk, uiData, uiErr) = await h.Ui.RequestAsync("ui", "ping");
        Assert.True(uiOk, uiErr);
        Assert.True(uiData.GetProperty("pong").GetBoolean());
    }

    [Fact]
    public async Task Eight_concurrent_ui_pipe_clients_are_served()
    {
        await using var h = new AgentHarness(install);
        var (agent, _) = await h.StartAndConnectUiAsync("c0");
        await h.SetupPassphraseAndWaitStoreAsync("c0");

        var names = Enumerable.Range(0, 8).Select(i => $"c{i}").ToArray();
        for (int i = 1; i < names.Length; i++)
        {
            var (ok, pid, errKind) = await h.Ui.ConnectAsync(names[i], h.Home, 3000);
            Assert.True(ok, $"Client {names[i]} failed to connect: {errKind}");
            Assert.Equal(agent.Id, pid);
        }

        var pingLines = await h.Ui.ParallelRequestAsync(names, "ping");
        Assert.Equal(8, pingLines.Length);
        foreach (var line in pingLines)
        {
            Assert.NotNull(line);
            var (ok, d, err) = Envelope.Parse(line!);
            Assert.True(ok, err);
            Assert.True(d.GetProperty("pong").GetBoolean());
        }

        var statusLines = await h.Ui.ParallelRequestAsync(names, "status");
        Assert.Equal(8, statusLines.Length);
        foreach (var line in statusLines)
        {
            Assert.NotNull(line);
            var (ok, d, err) = Envelope.Parse(line!);
            Assert.True(ok, err);
            var st = d.Deserialize(ContractsJson.Default.AgentStatus)!;
            Assert.False(st.Locked);
        }
    }

    [Fact]
    public async Task Quit_event_exits_zero_within_10s_stops_worker_child_and_removes_pid_file()
    {
        await using var h = new AgentHarness(install);
        var (agent, _) = await h.StartAndConnectUiAsync();
        await h.SetupPassphraseAndWaitStoreAsync();
        await h.WaitForWorkerWarmAsync();

        var workerChildren = ProcessTree.GetDirectChildren(agent.Id)
            .Where(c => string.Equals(c.ExeName, "BetterRecall.Worker.exe", StringComparison.OrdinalIgnoreCase))
            .ToList();
        var worker = Assert.Single(workerChildren);
        Assert.True(ProcessTree.IsAlive(worker.Pid));

        await h.Ui.CloseAsync("ui");
        Assert.True(h.SignalQuitEvent(), "Quit event could not be opened.");

        Assert.True(agent.WaitForExit(10_000), "Agent did not exit within 10 s after quit event.");
        Assert.Equal(0, agent.ExitCode);

        Assert.True(ProcessTree.WaitForExitByPid(worker.Pid, 5_000), "Worker child process did not exit after agent quit.");
        Assert.False(File.Exists(DataPaths.AgentPidFile(h.Home)), "agent.pid was not removed on clean shutdown.");

        var (afterOk, _, _) = await h.Ui.ConnectAsync("after", h.Home, 300);
        Assert.False(afterOk);
    }
}
