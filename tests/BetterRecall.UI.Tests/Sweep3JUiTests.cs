// SPDX-License-Identifier: MIT
// Copyright (c) BetterRecall contributors
using System.Text.Json.Nodes;
using BetterRecall.Contracts;
using BetterRecall.UI.Logic;

namespace BetterRecall.UI.Tests;

/// <summary>Bug sweep 3, batch J, UI logic.</summary>
public class Sweep3JUiTests
{
    /// <summary>D2: settings fetched after a reconnect become the new baseline; pending edits stay on top and a save
    /// sends only them.</summary>
    [Fact]
    public async Task D2_refresh_rebases_and_keeps_pending_edits()
    {
        await using var h = new MockHarness();
        var editor = new SettingsEditor();
        var first = (await h.Api.SettingsGetAsync()).Value!;
        editor.Load(first);
        editor.Set("capture.intervalMs", 3000);   // pending edit

        var fresh = (JsonObject)first.DeepClone();   // the restarted agent's settings differ
        SettingsSchema.Set(fresh, "security.excludeOwnWindows", JsonValue.Create(false));
        editor.Refresh(fresh);

        Assert.False(SettingsSchema.Get(editor.Original, "security.excludeOwnWindows")!.GetValue<bool>());
        Assert.Equal(3000, editor.Get("capture.intervalMs")!.GetValue<int>());
        Assert.Equal(["capture.intervalMs"], SettingsDiff.Keys(editor.Diff()));   // only the user's edit would be sent
    }

    [Fact]
    public void D2_refresh_before_load_loads()
    {
        var editor = new SettingsEditor();
        editor.Refresh(new JsonObject { ["capture"] = new JsonObject { ["intervalMs"] = 5000 } });
        Assert.True(editor.Loaded);
        Assert.False(editor.HasChanges);
    }

    /// <summary>D2: the subscription reports every (re)connect, so the UI can re-read settings.</summary>
    [Fact]
    public async Task D2_subscription_reports_connects()
    {
        await using var h = new MockHarness();
        await using var sub = new StatusSubscription(h.Server.PipeName, MockHarness.OwnProcessTrust());
        var connected = new TaskCompletionSource(TaskCreationOptions.RunContinuationsAsynchronously);
        sub.Connected += () => connected.TrySetResult();
        sub.Start();
        await connected.Task.WaitAsync(TimeSpan.FromSeconds(5));
    }

    private sealed class Dialog { public int Hidden; }

    /// <summary>C2: dialogs that show decrypted content are hidden on lock (all) and when their window hides (owner).</summary>
    [Fact]
    public void C2_open_dialogs_hide_on_lock_and_with_their_window()
    {
        var set = new OpenDialogs<Dialog>();
        object main = new(), launcher = new();
        var a = new Dialog();
        var b = new Dialog();
        var ha = set.Track(a, main);
        using (set.Track(b, launcher))
        {
            set.HideAll(d => d.Hidden++, main);       // main window hides
            Assert.Equal((1, 0), (a.Hidden, b.Hidden));
            set.HideAll(d => d.Hidden++);             // lock: every window
            Assert.Equal((2, 1), (a.Hidden, b.Hidden));
        }
        Assert.Equal(1, set.Count);                   // closed dialogs untracked
        ha.Dispose();
        set.HideAll(d => d.Hidden++);
        Assert.Equal((2, 1), (a.Hidden, b.Hidden));
        set.Track(new Dialog(), null).Dispose();
        set.HideAll(_ => throw new InvalidOperationException());   // a dialog already closing does not break the lock
    }
}
