// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Text.Json;
using BetterRecall.Agent.Conformance.Harness;
using BetterRecall.Contracts;

namespace BetterRecall.Agent.Conformance;

[Collection("AgentConformance")]
public sealed class UpdateTests(InstallFixture install)
{
    [Fact]
    public async Task Update_status_returns_default_state_on_ui_pipe_and_is_refused_on_public_pipe()
    {
        await using var h = new AgentHarness(install, s => s.Ipc.Enabled = true);
        await h.StartAndConnectUiAsync();
        await h.SetupPassphraseAndWaitStoreAsync();

        // UI pipe: update status returns auto=false and the running app version (no network).
        var (vOk, vData, vErr) = await h.Ui.RequestAsync("ui", "version");
        Assert.True(vOk, vErr);
        var version = vData.Deserialize(ContractsJson.Default.VersionReply);
        Assert.NotNull(version);

        var (ok, data, err) = await h.Ui.RequestAsync("ui", "update status");
        Assert.True(ok, err);
        var status = data.Deserialize(ContractsJson.Default.UpdateStatus);
        Assert.NotNull(status);
        Assert.False(status!.Auto);
        Assert.Equal(version!.App, status.CurrentVersion);
        Assert.Equal("idle", status.State);
        Assert.False(status.Available);

        // Public pipe: update is UI-only and must be refused.
        await using var pub = await h.WaitForPublicConnectAsync();
        var (pubOk, _, pubErr) = Envelope.Parse((await pub.RequestAsync("update status"))!);
        Assert.False(pubOk);
        Assert.Equal("Unknown command 'update'.", pubErr);
    }
}
