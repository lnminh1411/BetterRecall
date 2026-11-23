// SPDX-License-Identifier: MIT
// Copyright (c) BetterRecall contributors
using BetterRecall.Contracts;
using BetterRecall.UI.Logic;

namespace BetterRecall.UI.Tests;

public sealed class UnlockLogicTests
{
    [Theory]
    [InlineData("auto", "hello", false, UnlockMode.Hello)]
    [InlineData("auto", "password", false, UnlockMode.Password)]
    [InlineData("passphrase", "hello", false, UnlockMode.Passphrase)]
    [InlineData("passphrase", "password", false, UnlockMode.Password)]
    [InlineData("passphrase", "password", true, UnlockMode.Passphrase)]   // the key itself still needs its passphrase
    [InlineData("tpm", "hello", true, UnlockMode.Hello)]
    public void Unlock_mode_from_settings(string protection, string method, bool keyLocked, UnlockMode mode) =>
        Assert.Equal(mode, UnlockLogic.Mode(protection, method, keyLocked));

    /// <summary>B1/B8/B13/B15: unlock mode follows the agent status unlockNeeds field when present.</summary>
    [Theory]
    [InlineData("passphrase", "auto", "hello", UnlockMode.Passphrase)]
    [InlineData("password", "auto", "hello", UnlockMode.Password)]      // B13: no Hello, the password stands in
    [InlineData("hello", "auto", "password", UnlockMode.Hello)]         // B8/B15: no password file, Hello stands in
    [InlineData("hello", "passphrase", "hello", UnlockMode.Hello)]
    [InlineData(null, "passphrase", "hello", UnlockMode.Passphrase)]    // an agent without the field: settings decide
    [InlineData("none", "auto", "password", UnlockMode.Password)]
    public void Unlock_mode_from_status_field(string? needs, string keyProtection, string method, UnlockMode expected) =>
        Assert.Equal(expected, UnlockLogic.Mode(needs, keyProtection, method, keyNeedsPassphrase: false));

    [Fact]
    public void Retry_after_parses_the_agent_message()
    {
        Assert.Equal(30, UnlockLogic.RetryAfter("Too many attempts. Try again in 30 s."));
        Assert.Null(UnlockLogic.RetryAfter("Wrong password."));
        Assert.Null(UnlockLogic.RetryAfter(null));
    }

    /// <summary>B13: HelloMissing answer switches to asking for the password.</summary>
    [Fact]
    public void Hello_missing_answer_asks_for_the_password()
    {
        Assert.True(UnlockLogic.AsksForPassword(AgentErrors.HelloMissing));
        Assert.False(UnlockLogic.AsksForPassword(AgentErrors.UnlockCancelled));
    }

    /// <summary>B2: forgot-passphrase link is shown only in passphrase mode.</summary>
    [Theory]
    [InlineData(UnlockMode.Passphrase, true)]
    [InlineData(UnlockMode.Hello, false)]
    [InlineData(UnlockMode.Password, false)]
    public void Shows_forgot_passphrase_only_in_passphrase_mode(UnlockMode mode, bool expected) =>
        Assert.Equal(expected, UnlockLogic.ShowForgotPassphrase(mode));

    /// <summary>B3: passphrase choice is blocked when Hello is unavailable and unlockMethod is hello.</summary>
    [Theory]
    [InlineData(false, "hello", true)]
    [InlineData(false, null, true)]
    [InlineData(false, "password", false)]
    [InlineData(true, "hello", false)]
    public void Blocks_passphrase_choice_matches_hello_and_unlock_method(bool helloAvailable, string? method, bool blocked) =>
        Assert.Equal(blocked, UnlockLogic.BlocksPassphraseChoice(helloAvailable, method));
}
