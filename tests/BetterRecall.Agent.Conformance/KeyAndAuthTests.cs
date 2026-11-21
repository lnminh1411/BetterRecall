// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Text.Json;
using BetterRecall.Agent.Conformance.Harness;
using BetterRecall.Contracts;

namespace BetterRecall.Agent.Conformance;

[Collection("AgentConformance")]
public sealed class KeyAndAuthTests(InstallFixture install)
{
    [Fact]
    public async Task First_run_status_reports_locked_waiting_for_passphrase_and_no_password()
    {
        await using var h = new AgentHarness(install);
        await h.StartAndConnectUiAsync();

        var (ok, d, err) = await h.Ui.RequestAsync("ui", "status");
        Assert.True(ok, err);
        var st = d.Deserialize(ContractsJson.Default.AgentStatus)!;
        Assert.False(st.Capturing);
        Assert.True(st.Paused);
        Assert.Equal("consent", st.PauseReason);
        Assert.True(st.Locked);
        Assert.Null(st.UnlockedUntil);
        Assert.Equal("passphrase", st.UnlockNeeds);
        Assert.Equal(0, st.Store.Rows);
        Assert.Equal(0, st.Store.Vectors);

        var (pwOk, pwData, pwErr) = await h.Ui.RequestAsync("ui", "password");
        Assert.True(pwOk, pwErr);
        var pw = pwData.Deserialize(ContractsJson.Default.PasswordStatus)!;
        Assert.False(pw.Set);
        Assert.Equal(0, pw.RetryAfterS);

        var (sOk, _, sErr) = await h.Ui.RequestAsync("ui", "search acme");
        Assert.False(sOk);
        Assert.Equal("Store is locked.", sErr);
    }

    [Fact]
    public async Task Setup_passphrase_rejects_short_value_and_accepts_valid_passphrase()
    {
        await using var h = new AgentHarness(install);
        await h.StartAndConnectUiAsync();

        var (badOk, _, badErr) = await h.Ui.RequestAsync("ui", "unlock short");
        Assert.False(badOk);
        Assert.Equal("A new passphrase must be 8 to 1024 characters.", badErr);

        await h.SetupPassphraseAndWaitStoreAsync();

        var (ok, d, err) = await h.Ui.RequestAsync("ui", "status");
        Assert.True(ok, err);
        var st = d.Deserialize(ContractsJson.Default.AgentStatus)!;
        Assert.False(st.Locked);
        Assert.NotNull(st.UnlockedUntil);
        Assert.Equal("none", st.UnlockNeeds);
        Assert.True(File.Exists(DataPaths.KeyFile(h.Home)));
    }

    [Fact]
    public async Task Set_unlock_password_validates_length_and_updates_password_status()
    {
        await using var h = new AgentHarness(install);
        await h.StartAndConnectUiAsync();
        await h.SetupPassphraseAndWaitStoreAsync();

        var (shortOk, _, shortErr) = await h.Ui.RequestAsync("ui", "password set " + JsonSerializer.Serialize(new { @new = "12345" }));
        Assert.False(shortOk);
        Assert.Equal("Password must be 6 to 256 characters.", shortErr);

        await h.SetUnlockPasswordAsync();

        var (pwOk, pwData, pwErr) = await h.Ui.RequestAsync("ui", "password");
        Assert.True(pwOk, pwErr);
        var pw = pwData.Deserialize(ContractsJson.Default.PasswordStatus)!;
        Assert.True(pw.Set);
        Assert.Equal(0, pw.RetryAfterS);
        Assert.True(File.Exists(DataPaths.UnlockPasswordFile(h.Home)));
    }

    [Fact]
    public async Task Lock_closes_read_gate_and_requires_password_to_unlock()
    {
        await using var h = new AgentHarness(install);
        await h.StartAndConnectUiAsync();
        await h.SetupPassphraseAndWaitStoreAsync();
        await h.SetUnlockPasswordAsync();

        var (lockOk, _, lockErr) = await h.Ui.RequestAsync("ui", "lock");
        Assert.True(lockOk, lockErr);

        var (stOk, stData, stErr) = await h.Ui.RequestAsync("ui", "status");
        Assert.True(stOk, stErr);
        var st = stData.Deserialize(ContractsJson.Default.AgentStatus)!;
        Assert.True(st.Locked);
        Assert.Null(st.UnlockedUntil);
        Assert.Equal("password", st.UnlockNeeds);

        var (sOk, _, sErr) = await h.Ui.RequestAsync("ui", "search acme");
        Assert.False(sOk);
        Assert.Equal("Store is locked.", sErr);

        var (wrongOk, _, wrongErr) = await h.Ui.RequestAsync("ui", "unlock wrong-password-1");
        Assert.False(wrongOk);
        Assert.Equal("Wrong password.", wrongErr);

        var (uOk, uData, uErr) = await h.Ui.RequestAsync("ui", "unlock " + h.Password);
        Assert.True(uOk, uErr);
        var reply = uData.Deserialize(ContractsJson.Default.UnlockReply)!;
        Assert.True(reply.Unlocked);
        Assert.NotNull(reply.Until);

        var (st2Ok, st2Data, _) = await h.Ui.RequestAsync("ui", "status");
        Assert.True(st2Ok);
        var st2 = st2Data.Deserialize(ContractsJson.Default.AgentStatus)!;
        Assert.False(st2.Locked);
        Assert.Equal("none", st2.UnlockNeeds);
    }

    [Fact]
    public async Task Five_wrong_passwords_trigger_throttle_with_retry_counter()
    {
        await using var h = new AgentHarness(install);
        await h.StartAndConnectUiAsync();
        await h.SetupPassphraseAndWaitStoreAsync();
        await h.SetUnlockPasswordAsync();
        Assert.True((await h.Ui.RequestAsync("ui", "lock")).Ok);

        for (int i = 1; i <= 4; i++)
        {
            var (ok, _, err) = await h.Ui.RequestAsync("ui", $"unlock wrong-try-{i}");
            Assert.False(ok);
            Assert.Equal("Wrong password.", err);
        }

        var (pwBeforeOk, pwBeforeData, _) = await h.Ui.RequestAsync("ui", "password");
        Assert.True(pwBeforeOk);
        Assert.Equal(0, pwBeforeData.Deserialize(ContractsJson.Default.PasswordStatus)!.RetryAfterS);

        var (fifthOk, _, fifthErr) = await h.Ui.RequestAsync("ui", "unlock wrong-try-5");
        Assert.False(fifthOk);
        Assert.Matches(@"^Too many attempts\. Try again in \d+ s\.$", fifthErr);

        var (pwAfterOk, pwAfterData, _) = await h.Ui.RequestAsync("ui", "password");
        Assert.True(pwAfterOk);
        var retryAfter = pwAfterData.Deserialize(ContractsJson.Default.PasswordStatus)!.RetryAfterS;
        Assert.InRange(retryAfter, 1, 30);

        var (throttledOk, _, throttledErr) = await h.Ui.RequestAsync("ui", "unlock " + h.Password);
        Assert.False(throttledOk);
        Assert.Matches(@"^Too many attempts\. Try again in \d+ s\.$", throttledErr);
    }

    [Fact]
    public async Task Restart_requires_passphrase_and_rejects_wrong_passphrase()
    {
        await using var h = new AgentHarness(install);
        var (first, _) = await h.StartAndConnectUiAsync("ui1");
        await h.SetupPassphraseAndWaitStoreAsync("ui1");
        await h.SetUnlockPasswordAsync("ui1");
        await h.Ui.CloseAsync("ui1");
        await h.QuitAgentAsync(first);
        Assert.True(first.HasExited);

        await h.StartAndConnectUiAsync("ui2");

        var (stOk, stData, _) = await h.Ui.RequestAsync("ui2", "status");
        Assert.True(stOk);
        var st = stData.Deserialize(ContractsJson.Default.AgentStatus)!;
        Assert.True(st.Locked);
        Assert.Equal("passphrase", st.UnlockNeeds);

        var (badOk, _, badErr) = await h.Ui.RequestAsync("ui2", "unlock wrong-passphrase-attempt");
        Assert.False(badOk);
        Assert.Equal("Wrong passphrase.", badErr);

        await h.SetupPassphraseAndWaitStoreAsync("ui2");
        var (st2Ok, st2Data, _) = await h.Ui.RequestAsync("ui2", "status");
        Assert.True(st2Ok);
        var st2 = st2Data.Deserialize(ContractsJson.Default.AgentStatus)!;
        Assert.False(st2.Locked);
        Assert.Equal("none", st2.UnlockNeeds);
    }

    [Fact]
    public async Task Passphrase_verify_while_unlocked_checks_key_passphrase()
    {
        await using var h = new AgentHarness(install);
        await h.StartAndConnectUiAsync();
        await h.SetupPassphraseAndWaitStoreAsync();

        var (badOk, _, badErr) = await h.Ui.RequestAsync("ui", "passphrase verify " + JsonSerializer.Serialize(new { passphrase = "not-the-passphrase" }));
        Assert.False(badOk);
        Assert.Equal("Wrong passphrase.", badErr);

        var (goodOk, _, goodErr) = await h.Ui.RequestAsync("ui", "passphrase verify " + JsonSerializer.Serialize(new { passphrase = h.Passphrase }));
        Assert.True(goodOk, goodErr);
    }
}
