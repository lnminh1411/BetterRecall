// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Text.Json;
using BetterRecall.Agent.Conformance.Harness;
using BetterRecall.Contracts;

namespace BetterRecall.Agent.Conformance;

[Collection("AgentConformance")]
public sealed class SettingsTests(InstallFixture install)
{
    [Fact]
    public async Task Settings_get_locked_guards_and_reset_when_valid()
    {
        await using var h = new AgentHarness(install);
        await h.StartAndConnectUiAsync();

        // was: Settings_get_returns_full_configuration_shape
        var (ok, d, err) = await h.Ui.RequestAsync("ui", "settings get");
        Assert.True(ok, err);
        var s = d.Deserialize(ContractsJson.Default.Settings)!;
        Assert.False(s.Capture.Enabled);
        Assert.False(s.Keyframes.Enabled);
        Assert.False(s.Replay.Enabled);
        Assert.False(s.Ocr.Enabled);
        Assert.Equal("passphrase", s.Security.KeyProtection);
        Assert.Equal("password", s.Security.UnlockMethod);
        Assert.False(s.Ipc.Enabled);
        Assert.False(s.Mcp.Enabled);

        // was: Settings_set_guards_protected_groups_while_locked
        var (guardedOk, _, guardedErr) = await h.Ui.RequestAsync("ui", "settings set {\"retention\":{\"days\":45}}");
        Assert.False(guardedOk);
        Assert.Equal("Store is locked.", guardedErr);

        var (uiOk, uiData, uiErr) = await h.Ui.RequestAsync("ui", "settings set {\"ui\":{\"theme\":\"dark\"}}");
        Assert.True(uiOk, uiErr);
        Assert.Equal("dark", uiData.Deserialize(ContractsJson.Default.Settings)!.Ui.Theme);

        // was: Settings_reset_reports_nothing_to_reset_when_file_is_valid
        var (lockedOk, _, lockedErr) = await h.Ui.RequestAsync("ui", "settings reset");
        Assert.False(lockedOk);
        Assert.Equal("Store is locked.", lockedErr);

        await h.SetupPassphraseAndWaitStoreAsync();

        var (resetOk, _, resetErr) = await h.Ui.RequestAsync("ui", "settings reset");
        Assert.False(resetOk);
        Assert.Equal("settings.json loaded; there is nothing to reset.", resetErr);
    }

    [Fact]
    public async Task Settings_set_updates_valid_key_persists_to_disk_and_survives_restart()
    {
        await using var h = new AgentHarness(install);
        var (first, _) = await h.StartAndConnectUiAsync("ui1");
        await h.SetupPassphraseAndWaitStoreAsync("ui1");

        var (ok, d, err) = await h.Ui.RequestAsync("ui1", "settings set {\"retention\":{\"days\":90}}");
        Assert.True(ok, err);
        Assert.Equal(90, d.Deserialize(ContractsJson.Default.Settings)!.Retention.Days);

        var onDisk = JsonSerializer.Deserialize(File.ReadAllText(DataPaths.SettingsFile(h.Home)), ContractsJson.Default.Settings)!;
        Assert.Equal(90, onDisk.Retention.Days);

        await h.Ui.CloseAsync("ui1");
        await h.QuitAgentAsync(first);

        await h.StartAndConnectUiAsync("ui2");
        var (getOk, getData, getErr) = await h.Ui.RequestAsync("ui2", "settings get");
        Assert.True(getOk, getErr);
        Assert.Equal(90, getData.Deserialize(ContractsJson.Default.Settings)!.Retention.Days);
    }

    [Fact]
    public async Task Settings_set_rejects_out_of_range_unknown_keys_and_bad_json()
    {
        await using var h = new AgentHarness(install);
        await h.StartAndConnectUiAsync();
        await h.SetupPassphraseAndWaitStoreAsync();

        // was: Settings_set_rejects_out_of_range_values
        var (rOk, _, rErr) = await h.Ui.RequestAsync("ui", "settings set {\"retention\":{\"days\":0}}");
        Assert.False(rOk);
        Assert.Equal("Invalid retention.days.", rErr);

        var (cOk, _, cErr) = await h.Ui.RequestAsync("ui", "settings set {\"capture\":{\"intervalMs\":100}}");
        Assert.False(cOk);
        Assert.Equal("Invalid capture.intervalMs.", cErr);

        var (pOk, _, pErr) = await h.Ui.RequestAsync("ui", "settings set {\"ipc\":{\"pipeName\":\"BetterRecall.ui\"}}");
        Assert.False(pOk);
        Assert.Equal("Invalid ipc.pipeName.", pErr);

        var (gOk, gData, _) = await h.Ui.RequestAsync("ui", "settings get");
        Assert.True(gOk);
        var current = gData.Deserialize(ContractsJson.Default.Settings)!;
        Assert.Equal(30, current.Retention.Days);
        Assert.Equal(2000, current.Capture.IntervalMs);
        Assert.Equal("BetterRecall", current.Ipc.PipeName);

        // was: Settings_set_rejects_unknown_keys_sections_and_bad_json
        var (kOk, _, kErr) = await h.Ui.RequestAsync("ui", "settings set {\"retention\":{\"noSuchKey\":1}}");
        Assert.False(kOk);
        Assert.Equal("Invalid retention.noSuchKey.", kErr);

        var (sOk, _, sErr) = await h.Ui.RequestAsync("ui", "settings set {\"noSuchSection\":{\"x\":1}}");
        Assert.False(sOk);
        Assert.Equal("Invalid noSuchSection.", sErr);

        var (jOk, _, jErr) = await h.Ui.RequestAsync("ui", "settings set {not json");
        Assert.False(jOk);
        Assert.Equal("Invalid JSON.", jErr);

        var (uOk, _, uErr) = await h.Ui.RequestAsync("ui", "settings");
        Assert.False(uOk);
        Assert.Equal("Usage: settings get | settings set <json> | settings reset", uErr);
    }
}
