// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Text.Json;
using BetterRecall.Agent.Conformance.Harness;
using BetterRecall.Contracts;

namespace BetterRecall.Agent.Conformance;

[Collection("AgentConformance")]
public sealed class PurgeTests(InstallFixture install)
{
    [Fact]
    public async Task Purge_all_on_empty_store_requires_recent_unlock_and_clears_key()
    {
        await using var h = new AgentHarness(install);
        await h.StartAndConnectUiAsync();

        var (reqOk, _, reqErr) = await h.Ui.RequestAsync("ui", "purge all");
        Assert.False(reqOk);
        Assert.Equal("Unlock required.", reqErr);

        await h.SetupPassphraseAndWaitStoreAsync();

        var (pOk, pData, pErr) = await h.Ui.RequestAsync("ui", "purge all");
        Assert.True(pOk, pErr);
        Assert.False(pData.TryGetProperty("rows", out _));
        var confirm = pData.Deserialize(ContractsJson.Default.PurgeConfirmReply)!;
        Assert.False(string.IsNullOrWhiteSpace(confirm.Confirm));
        Assert.Null(confirm.Rows);

        var (cOk, cData, cErr) = await h.Ui.RequestAsync("ui", "purge confirm " + confirm.Confirm);
        Assert.True(cOk, cErr);
        Assert.False(cData.TryGetProperty("undeleted", out _));
        var result = cData.Deserialize(ContractsJson.Default.PurgeResult)!;
        Assert.Equal(0, result.Purged);
        Assert.Null(result.Undeleted);

        Assert.False(File.Exists(DataPaths.KeyFile(h.Home)));

        var (stOk, stData, stErr) = await h.Ui.RequestAsync("ui", "status");
        Assert.True(stOk, stErr);
        var st = stData.Deserialize(ContractsJson.Default.AgentStatus)!;
        Assert.True(st.Locked);
        Assert.Equal("passphrase", st.UnlockNeeds);
        Assert.Equal(0, st.Store.Rows);

        // A fresh passphrase re-initializes the key and reopens the empty store.
        await h.SetupPassphraseAndWaitStoreAsync(passphrase: h.Passphrase + "-new");
    }

    [Fact]
    public async Task Purge_scoped_on_empty_store_enforces_token_rules_and_argument_errors()
    {
        await using var h = new AgentHarness(install);
        await h.StartAndConnectUiAsync("ui1");
        await h.SetupPassphraseAndWaitStoreAsync("ui1");

        // was: Purge_scoped_on_empty_store_returns_zero_and_enforces_token_rules
        var (conn2Ok, _, _) = await h.Ui.ConnectAsync("ui2", h.Home);
        Assert.True(conn2Ok);

        var (pOk, pData, pErr) = await h.Ui.RequestAsync("ui1", "purge last 60");
        Assert.True(pOk, pErr);
        var confirm = pData.Deserialize(ContractsJson.Default.PurgeConfirmReply)!;
        Assert.Equal(0, confirm.Rows);
        Assert.False(string.IsNullOrWhiteSpace(confirm.Confirm));

        // Token is bound to the connection that created it.
        var (otherOk, _, otherErr) = await h.Ui.RequestAsync("ui2", "purge confirm " + confirm.Confirm);
        Assert.False(otherOk);
        Assert.Equal("Invalid or expired token.", otherErr);

        var (cOk, cData, cErr) = await h.Ui.RequestAsync("ui1", "purge confirm " + confirm.Confirm);
        Assert.True(cOk, cErr);
        var result = cData.Deserialize(ContractsJson.Default.PurgeResult)!;
        Assert.Equal(0, result.Purged);
        Assert.Null(result.Undeleted);

        // Token is single-use.
        var (reuseOk, _, reuseErr) = await h.Ui.RequestAsync("ui1", "purge confirm " + confirm.Confirm);
        Assert.False(reuseOk);
        Assert.Equal("Invalid or expired token.", reuseErr);

        // was: Purge_usage_and_argument_errors_match_documented_strings
        const string usage = "Usage: purge last <seconds> | purge range <from> <to> | purge app <name> | purge site <host> | purge id <id> | purge all | purge confirm <token>";
        Assert.Equal(usage, (await h.Ui.RequestAsync("ui1", "purge")).Error);
        Assert.Equal(usage, (await h.Ui.RequestAsync("ui1", "purge range bad1 bad2")).Error);
        Assert.Equal("Invalid seconds.", (await h.Ui.RequestAsync("ui1", "purge last 0")).Error);
        Assert.Equal("Invalid from.", (await h.Ui.RequestAsync("ui1", "purge range 2026-11-10T00:00:00Z 2026-11-09T00:00:00Z")).Error);
    }
}
