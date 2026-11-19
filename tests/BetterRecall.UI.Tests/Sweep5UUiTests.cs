// SPDX-License-Identifier: MIT
// Copyright (c) BetterRecall contributors
using System.Diagnostics;
using System.Text.Json;
using System.Text.Json.Nodes;
using BetterRecall.Contracts;
using BetterRecall.UI.Logic;

namespace BetterRecall.UI.Tests;

/// <summary>Bug sweep 5, batch U: MIT UI Logic tests for B2, B3, B4, B6 and C6 (no backend references; real-agent seam tests live in Core.Tests).</summary>
public sealed class Sweep5UUiTests
{
    private static AgentClient ClientFor(FakeLineServer srv) =>
        new(srv.Name, new ServerTrustVerifier(Environment.ProcessPath!));

    // ---- B2: forgot passphrase link & PurgeAll flow ----

    [Theory]
    [InlineData(UnlockMode.Passphrase, true)]
    [InlineData(UnlockMode.Hello, false)]
    [InlineData(UnlockMode.Password, false)]
    public void B2_unlock_logic_shows_forgot_passphrase_only_in_passphrase_mode(UnlockMode mode, bool expected) =>
        Assert.Equal(expected, UnlockLogic.ShowForgotPassphrase(mode));

    [Fact]
    public async Task B2_purge_flow_proceeds_when_hello_missing_on_empty_unlock_and_aborts_on_hello_cancel()
    {
        await using var noHelloSrv = new FakeLineServer(line => line switch
        {
            "unlock" => Envelope.Error(AgentErrors.HelloMissing),
            "purge all" => Envelope.Ok("{\"confirm\":\"tok-1\"}"),
            "purge confirm tok-1" => Envelope.Ok("{\"purged\":5}"),
            _ => Envelope.Error("unexpected"),
        });
        await using (var c = ClientFor(noHelloSrv))
        {
            var r = await PurgeFlow.PurgeAllAsync(new AgentApi(c), null);
            Assert.True(r.Ok, r.Error);
            Assert.Equal(5, r.Value!.Purged);
        }

        await using var cancelSrv = new FakeLineServer(line => line switch
        {
            "unlock" => Envelope.Error("Unlock cancelled."),
            _ => Envelope.Error("should not be reached"),
        });
        await using (var c = ClientFor(cancelSrv))
        {
            var r = await PurgeFlow.PurgeAllAsync(new AgentApi(c), null);
            Assert.False(r.Ok);
            Assert.Equal("Unlock cancelled.", r.Error);
            Assert.Single(cancelSrv.Received);
        }
    }

    // ---- B3 (D2): passphrase choice blocked when Hello unavailable & unlockMethod is hello ----

    [Theory]
    [InlineData(false, "hello", true)]
    [InlineData(false, null, true)]
    [InlineData(false, "password", false)]
    [InlineData(true, "hello", false)]
    public void B3_blocks_passphrase_choice_matches_hello_and_unlock_method(bool helloAvailable, string? method, bool blocked) =>
        Assert.Equal(blocked, UnlockLogic.BlocksPassphraseChoice(helloAvailable, method));

    // ---- B4 (D3): inline passphrase prompt during Settings save ----

    [Fact]
    public async Task B4_settings_save_flow_verifies_passphrase_inline_and_stops_on_cancel()
    {
        bool verified = false;
        await using var srv = new FakeLineServer(line =>
        {
            if (line.StartsWith("settings set ", StringComparison.Ordinal))
                return verified
                    ? Envelope.Ok("{\"security\":{\"keyProtection\":\"dpapi\",\"unlockMethod\":\"password\"}}")
                    : Envelope.Error(AgentErrors.PassphraseNeeded);
            if (line == "passphrase verify {\"passphrase\":\"correct-pass\"}")
            {
                verified = true;
                return Envelope.Ok("null");
            }
            return Envelope.Error(AgentErrors.WrongPassphrase);
        });
        await using var client = ClientFor(srv);
        var api = new AgentApi(client);

        var editor = new SettingsEditor();
        editor.Load(JsonNode.Parse("{\"security\":{\"keyProtection\":\"passphrase\",\"unlockMethod\":\"password\"}}")!.AsObject());
        editor.Set("security.keyProtection", "dpapi");

        // Cancelling the prompt returns Failed with PassphraseNeeded and does not retry save.
        var cancelled = await SettingsSaveFlow.SaveWithPassphraseAsync(editor, api, _ => Task.FromResult(false));
        Assert.Equal(SaveOutcome.Failed, cancelled.Outcome);
        Assert.Equal(AgentErrors.PassphraseNeeded, cancelled.Last?.Error);

        // Verifying inline retries and saves without leaving Settings.
        var saved = await SettingsSaveFlow.SaveWithPassphraseAsync(editor, api, async v => (await v("correct-pass")).Ok);
        Assert.Equal(SaveOutcome.Saved, saved.Outcome);
    }

    // ---- B6: ChatViewModel.ApplyRemoved prunes cited answers, approval cards, and full wipes ----

    [Fact]
    public void B6_chat_view_model_apply_removed_clears_cited_messages_truncated_all_and_approvals()
    {
        var t0 = DateTimeOffset.UtcNow;

        // 1) Unrelated removed event leaves chat intact.
        var vm = new ChatViewModel();
        vm.Load(new ChatDetail(new ChatSummary("c1", "Test", t0, t0, 2),
        [
            new ChatMessage(1, t0, "user", "hi"),
            new ChatMessage(2, t0, "assistant", "there", [new ChatSource("cap-1", t0, CaptureKind.Screen, "notepad.exe", "t", "")]),
        ], Approved: true));
        Assert.False(vm.ApplyRemoved(new RemovedEvent("removed", "purge", ["cap-other"])));
        Assert.Equal("c1", vm.ChatId);
        Assert.Equal(2, vm.Messages.Count);

        // 2) Cited capture removed resets the chat.
        Assert.True(vm.ApplyRemoved(new RemovedEvent("removed", "purge", ["cap-1"])));
        Assert.Null(vm.ChatId);
        Assert.Empty(vm.Messages);

        // 3) Pending approval mentioning a removed capture drops the card and rolls back the open turn.
        var apVm = new ChatViewModel();
        apVm.BeginSend("summarize this");
        apVm.Handle(new ChatEvent("approval", Token: "tok-a", Share: "text", Items:
        [
            new ChatApprovalItem("cap-2", t0, CaptureKind.Screen, "code.exe", "main.cs", "", "hello"),
        ]));
        Assert.True(apVm.HasApproval);
        Assert.True(apVm.ApplyRemoved(new RemovedEvent("removed", "purge", ["cap-2"])));
        Assert.False(apVm.HasApproval);
        Assert.Empty(apVm.Messages);
        Assert.False(apVm.Busy);

        // 4) "all" or Truncated clears any loaded chat.
        var trVm = new ChatViewModel();
        trVm.Load(new ChatDetail(new ChatSummary("c2", "Keep?", t0, t0, 1), [new ChatMessage(1, t0, "user", "q")], Approved: true));
        Assert.True(trVm.ApplyRemoved(new RemovedEvent("removed", "all", [])));
        Assert.Empty(trVm.Messages);
    }

    // ---- C6: UI exits when agent died even if exit happened while a window was visible ----

    [Fact]
    public void C6_exit_policy_and_agent_watch_track_dead_agent_across_window_dismissal()
    {
        Assert.False(UiExitPolicy.ShouldExit(agentExited: false, launcherVisible: false, mainVisible: false));
        Assert.False(UiExitPolicy.ShouldExit(agentExited: true, launcherVisible: true, mainVisible: false));
        Assert.False(UiExitPolicy.ShouldExit(agentExited: true, launcherVisible: false, mainVisible: true));
        Assert.True(UiExitPolicy.ShouldExit(agentExited: true, launcherVisible: false, mainVisible: false));

        using var watch = new AgentExitWatch();
        int exited = 0;
        watch.Exited += () => Interlocked.Increment(ref exited);

        // Watching a dead/non-existent PID marks HasExited and !IsAlive().
        watch.Watch(int.MaxValue);
        Assert.True(watch.HasExited);
        Assert.False(watch.IsAlive());
        Assert.Equal(1, exited);

        // Re-attaching to a live process resets _raised / HasExited cleanly.
        watch.Watch(Environment.ProcessId);
        Assert.False(watch.HasExited);
        Assert.True(watch.IsAlive());
    }
}
