// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Text.Json;
using BetterRecall.Agent.Conformance.Harness;
using BetterRecall.Contracts;
using BetterRecall.Core.Ipc;

namespace BetterRecall.Agent.Conformance;

[Collection("AgentConformance")]
public sealed class PublicIpcTests(InstallFixture install)
{
    [Fact]
    public async Task Public_pipe_is_off_by_default_and_toggles_live_via_settings()
    {
        await using var h = new AgentHarness(install);
        var (agent, _) = await h.StartAndConnectUiAsync();
        await h.SetupPassphraseAndWaitStoreAsync();

        var (offRes, offClient) = await h.ConnectPublicAsync(300);
        if (offClient is not null) await offClient.DisposeAsync();
        Assert.Equal(PipeConnectResult.NotRunning, offRes);

        var (cliOffExit, cliOffOut, _) = await h.RunCliAsync("ipc", "ping");
        Assert.Equal(3, cliOffExit);
        Assert.Contains("Scripts (IPC) are off", cliOffOut);

        Assert.True((await h.Ui.RequestAsync("ui", "settings set {\"ipc\":{\"enabled\":true}}")).Ok);
        await using (var pub = await h.WaitForPublicConnectAsync())
        {
            Assert.Equal(agent.Id, pub.ServerPid);
            var (pingOk, pingData, _) = Envelope.Parse((await pub.RequestAsync("ping"))!);
            Assert.True(pingOk);
            Assert.True(pingData.GetProperty("pong").GetBoolean());

            var (verOk, verData, _) = Envelope.Parse((await pub.RequestAsync("version"))!);
            Assert.True(verOk);
            Assert.Equal(1, verData.GetProperty("protocol").GetInt32());

            var (helpOk, helpData, _) = Envelope.Parse((await pub.RequestAsync("help"))!);
            Assert.True(helpOk);
            var verbs = helpData.GetProperty("verbs").EnumerateArray().Select(e => e.GetString()).ToList();
            Assert.Contains("ping", verbs);
            Assert.Contains("search", verbs);
            Assert.DoesNotContain("pause", verbs);
            Assert.DoesNotContain("thumb", verbs);
            Assert.DoesNotContain("purge", verbs);
        }

        Assert.True((await h.Ui.RequestAsync("ui", "settings set {\"ipc\":{\"enabled\":false}}")).Ok);
        await h.WaitForPublicClosedAsync();
    }

    [Fact]
    public async Task Public_pipe_allowClients_filters_connecting_executables()
    {
        await using var h = new AgentHarness(install, s => s.Ipc.Enabled = true);
        await h.StartAndConnectUiAsync();
        await h.SetupPassphraseAndWaitStoreAsync();

        Assert.True((await h.Ui.RequestAsync("ui", "settings set {\"ipc\":{\"allowClients\":[\"C:\\\\Tools\\\\other.exe\"]}}")).Ok);
        await Task.Delay(150);

        await using (var blocked = await h.WaitForPublicConnectAsync())
        {
            var line = await blocked.ReadLineAsync();
            Assert.Equal("{\"ok\":false,\"error\":\"Client not allowed.\"}", line);
            Assert.Null(await blocked.ReadLineAsync());
        }

        var self = Environment.ProcessPath!;
        Assert.True((await h.Ui.RequestAsync("ui", "settings set " + JsonSerializer.Serialize(new { ipc = new { allowClients = new[] { self } } }))).Ok);
        await Task.Delay(150);

        await using var allowed = await h.WaitForPublicConnectAsync();
        var (ok, d, err) = Envelope.Parse((await allowed.RequestAsync("ping"))!);
        Assert.True(ok, err);
        Assert.True(d.GetProperty("pong").GetBoolean());
    }

    [Fact]
    public async Task Public_pipe_requireUnlocked_refuses_while_locked_and_allows_when_disabled()
    {
        await using var h = new AgentHarness(install, s => s.Ipc.Enabled = true);
        await h.StartAndConnectUiAsync();
        await h.SetupPassphraseAndWaitStoreAsync();
        await h.SetUnlockPasswordAsync();
        Assert.True((await h.Ui.RequestAsync("ui", "lock")).Ok);

        await using var pub = await h.WaitForPublicConnectAsync();

        var (sOk, _, sErr) = Envelope.Parse((await pub.RequestAsync("search acme"))!);
        Assert.False(sOk);
        Assert.Equal("Store is locked.", sErr);

        var (gOk, _, gErr) = Envelope.Parse((await pub.RequestAsync("get s_missing"))!);
        Assert.False(gOk);
        Assert.Equal("Store is locked.", gErr);

        var (stOk, stData, _) = Envelope.Parse((await pub.RequestAsync("status"))!);
        Assert.True(stOk);
        Assert.False(stData.TryGetProperty("unlockNeeds", out _));
        var st = stData.Deserialize(ContractsJson.Default.AgentStatus)!;
        Assert.True(st.Locked);
        Assert.Equal(0, st.Store.Rows);

        Assert.True((await h.Ui.RequestAsync("ui", "unlock " + h.Password)).Ok);
        Assert.True((await h.Ui.RequestAsync("ui", "settings set {\"ipc\":{\"requireUnlocked\":false}}")).Ok);
        Assert.True((await h.Ui.RequestAsync("ui", "lock")).Ok);

        // UI pipe still sees the store as locked, while the public pipe may search.
        Assert.Equal("Store is locked.", (await h.Ui.RequestAsync("ui", "search {\"q\":\"acme\",\"mode\":\"text\"}")).Error);
        var (pubOk, pubData, pubErr) = Envelope.Parse((await pub.RequestAsync("search {\"q\":\"acme\",\"mode\":\"text\"}"))!);
        Assert.True(pubOk, pubErr);
        Assert.Equal(0, pubData.Deserialize(ContractsJson.Default.SearchResponse)!.Count);
    }

    [Fact]
    public async Task Public_pipe_verb_table_and_documented_errors()
    {
        await using var h = new AgentHarness(install, s => s.Ipc.Enabled = true);
        await h.StartAndConnectUiAsync();
        await h.SetupPassphraseAndWaitStoreAsync();
        await h.WaitForWorkerWarmAsync();

        await using var pub = await h.WaitForPublicConnectAsync();

        Assert.Equal("Unknown command 'frobnicate'.", Envelope.Parse((await pub.RequestAsync("frobnicate now"))!).Error);
        Assert.Equal("Unknown command 'settings'.", Envelope.Parse((await pub.RequestAsync("settings get"))!).Error);
        Assert.Equal("Unknown command ''.", Envelope.Parse((await pub.RequestAsync(""))!).Error);

        Assert.Equal("Control is off (config ipc.allowControl).", Envelope.Parse((await pub.RequestAsync("pause"))!).Error);
        Assert.Equal("Control is off (config ipc.allowControl).", Envelope.Parse((await pub.RequestAsync("resume"))!).Error);
        Assert.Equal("Thumbnails are off (config ipc.allowThumbnails).", Envelope.Parse((await pub.RequestAsync("thumb s_1"))!).Error);
        Assert.Equal("Control is off (config ipc.allowControl).", Envelope.Parse((await pub.RequestAsync("purge last 60"))!).Error);

        Assert.True((await h.Ui.RequestAsync("ui", "settings set {\"ipc\":{\"allowControl\":true}}")).Ok);
        Assert.Equal("Purge is off (config ipc.allowPurge).", Envelope.Parse((await pub.RequestAsync("purge last 60"))!).Error);

        Assert.Equal("Usage: search <text> | search <json>", Envelope.Parse((await pub.RequestAsync("search"))!).Error);
        Assert.Equal("Invalid JSON.", Envelope.Parse((await pub.RequestAsync("search {bad json"))!).Error);
        Assert.Equal("Invalid q.", Envelope.Parse((await pub.RequestAsync("search {\"limit\":3}"))!).Error);
        Assert.Equal("Invalid limit.", Envelope.Parse((await pub.RequestAsync("search {\"q\":\"x\",\"limit\":0}"))!).Error);
        Assert.Equal("Invalid kind.", Envelope.Parse((await pub.RequestAsync("search {\"q\":\"x\",\"kind\":[\"video\"]}"))!).Error);
        Assert.Equal("Invalid mode.", Envelope.Parse((await pub.RequestAsync("search {\"q\":\"x\",\"mode\":\"fuzzy\"}"))!).Error);
        Assert.Equal("Usage: get <id>", Envelope.Parse((await pub.RequestAsync("get"))!).Error);
        Assert.Equal("Not found.", Envelope.Parse((await pub.RequestAsync("get nope"))!).Error);
        Assert.Equal("Usage: subscribe [captures]", Envelope.Parse((await pub.RequestAsync("subscribe everything"))!).Error);
    }

    [Fact]
    public async Task Public_pipe_resume_clears_only_ipc_pause()
    {
        await using var h = new AgentHarness(install, s =>
        {
            s.Ipc.Enabled = true;
            s.Ipc.AllowControl = true;
        });
        await h.StartAndConnectUiAsync();
        await h.SetupPassphraseAndWaitStoreAsync();

        await using var pub = await h.WaitForPublicConnectAsync();

        // Pure IPC pause is cleared by IPC resume.
        Assert.True(Envelope.Parse((await pub.RequestAsync("pause"))!).Ok);
        Assert.True((await UiStatusAsync(h)).UserPaused);
        Assert.True(Envelope.Parse((await pub.RequestAsync("resume"))!).Ok);
        Assert.False((await UiStatusAsync(h)).UserPaused);

        // User pause + IPC pause: IPC resume clears only the IPC pause, preserving the user pause.
        Assert.True((await h.Ui.RequestAsync("ui", "pause")).Ok);
        Assert.True(Envelope.Parse((await pub.RequestAsync("pause"))!).Ok);
        Assert.True((await UiStatusAsync(h)).UserPaused);

        Assert.True(Envelope.Parse((await pub.RequestAsync("resume"))!).Ok);
        Assert.True((await UiStatusAsync(h)).UserPaused);

        Assert.True((await h.Ui.RequestAsync("ui", "resume")).Ok);
        Assert.False((await UiStatusAsync(h)).UserPaused);
    }

    [Fact]
    public async Task Cli_executes_public_ipc_commands_against_running_agent()
    {
        await using var h = new AgentHarness(install, s =>
        {
            s.Ipc.Enabled = true;
            s.Ipc.RequireUnlocked = false;
        });
        await h.StartAndConnectUiAsync();
        await h.SetupPassphraseAndWaitStoreAsync();

        var (pingExit, pingOut, _) = await h.RunCliAsync("ipc", "ping");
        Assert.Equal(0, pingExit);
        var (okPing, pingData, _) = Envelope.Parse(pingOut.Trim());
        Assert.True(okPing);
        Assert.True(pingData.GetProperty("pong").GetBoolean());

        var (searchExit, searchOut, _) = await h.RunCliAsync("ipc", "search", "{\"q\":\"acme\",\"mode\":\"text\"}");
        Assert.Equal(0, searchExit);
        var (okSearch, searchData, _) = Envelope.Parse(searchOut.Trim());
        Assert.True(okSearch);
        Assert.Equal(0, searchData.Deserialize(ContractsJson.Default.SearchResponse)!.Count);
    }

    private static async Task<AgentStatus> UiStatusAsync(AgentHarness h)
    {
        var (ok, d, err) = await h.Ui.RequestAsync("ui", "status");
        Assert.True(ok, err);
        return d.Deserialize(ContractsJson.Default.AgentStatus)!;
    }
}
