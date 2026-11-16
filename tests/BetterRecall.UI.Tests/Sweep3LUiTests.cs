// SPDX-License-Identifier: MIT
// Copyright (c) BetterRecall contributors
using BetterRecall.Contracts;
using BetterRecall.UI.Logic;
using Xunit;

namespace BetterRecall.UI.Tests;

/// <summary>Bug sweep 3, batch L: UI logic behind the visual, accessibility and copy fixes.</summary>
public sealed class Sweep3LUiTests
{
    private static readonly DateTimeOffset T = new(2026, 11, 16, 9, 30, 0, TimeSpan.Zero);

    private static Dictionary<string, string> Strings()
    {
        var dir = new DirectoryInfo(AppContext.BaseDirectory);
        while (dir is not null && !File.Exists(Path.Combine(dir.FullName, "BetterRecall.slnx"))) dir = dir.Parent;
        Assert.NotNull(dir);
        var resw = System.Xml.Linq.XDocument.Load(Path.Combine(dir!.FullName, "src", "BetterRecall.UI", "Strings", "en-US", "Resources.resw"));
        return resw.Root!.Elements("data").ToDictionary(e => (string)e.Attribute("name")!, e => (string)e.Element("value")!);
    }

    private static ChatDetail Chat(string id) =>
        new(new ChatSummary(id, "Chat " + id, T, T, 1), [new ChatMessage(1, T, "user", "hello " + id)], false);

    private static AgentStatus Status(string device = "NPU", int pending = 3, bool audio = true) =>
        new(Capturing: true, Paused: false, PausedUntil: null, PauseReason: null, Locked: false, UnlockedUntil: null,
            Audio: new AudioState(audio, false), Queue: new QueueState(pending), Device: device,
            Store: new StoreStats(808, 1 << 30, 0, null, null, 0, 0), RetentionDays: 30);

    // ---------------- C7: a late saved-chat fetch must not replace New chat or a later open

    [Fact]
    public void Late_open_after_new_chat_is_dropped()
    {
        var vm = new ChatViewModel();
        var ticket = vm.BeginOpen();   // chat A clicked, its fetch in flight
        vm.Reset();                    // New chat
        Assert.False(vm.LoadIfCurrent(ticket, Chat("A")));
        Assert.Null(vm.ChatId);        // the next question starts a new chat, not one appended to A
        Assert.Empty(vm.Messages);
    }

    [Fact]
    public void Only_the_latest_open_loads()
    {
        var vm = new ChatViewModel();
        var a = vm.BeginOpen();
        var b = vm.BeginOpen();
        // the agent client is FIFO, so A answers first; it was superseded by B and must not show
        Assert.False(vm.LoadIfCurrent(a, Chat("A")));
        Assert.True(vm.LoadIfCurrent(b, Chat("B")));
        Assert.Equal("B", vm.ChatId);
    }

    // ---------------- D6: saved-chat item name

    [Fact]
    public void Saved_chat_name_is_title_and_date_not_the_record()
    {
        var name = ChatViewModel.SummaryName(new ChatSummary("mc_sample_tokyo", "Flight to Tokyo", T, T, 2));
        Assert.StartsWith("Flight to Tokyo, ", name);
        Assert.DoesNotContain("mc_sample_tokyo", name);
        Assert.DoesNotContain("ChatSummary", name);
        Assert.Contains(T.ToLocalTime().ToString("g", System.Globalization.CultureInfo.CurrentCulture), name);
    }

    // ---------------- D18: copy

    [Fact]
    public void Pending_count_has_a_singular()
    {
        var s = Strings();
        Assert.Equal("Settings_PendingOne", SettingsDiff.PendingKey(1));
        Assert.Equal("Settings_Pending", SettingsDiff.PendingKey(2));
        Assert.Equal("1 unsaved change", s[SettingsDiff.PendingKey(1)]);
        Assert.Contains("{0}", s[SettingsDiff.PendingKey(3)]);
        Assert.Contains("letters, digits, dots, dashes or underscores", s["Settings_PipeNameError"]);
    }

    // ---------------- T1: consent and security copy

    [Fact]
    public void Consent_copy_matches_the_defaults()
    {
        var s = Strings();
        Assert.DoesNotContain("No text recognition", s["Consent_What4.Text"]);
        Assert.Contains("OCR", s["Consent_What4.Text"]);
        Assert.NotEqual("• BetterRecall makes no network connections.", s["Consent_Where3.Text"]);
        Assert.Contains("Cloud describe is off", s["Consent_Where3.Text"]);
        Assert.DoesNotContain("Windows Hello", s["S_security_requireUnlock"]);   // also true with the Password method
        Assert.DoesNotContain("Windows Hello", s["W_security_requireUnlock"]);
    }

    // ---------------- D17: raw "none" device and D16: status parts at narrow widths

    [Fact]
    public void Device_none_reads_not_loaded()
    {
        Assert.Equal(new LocalizedText("Status_DeviceNotLoaded"), StatusText.Device("none"));
        Assert.Equal(new LocalizedText("Status_DeviceNotLoaded"), StatusText.Device(""));
        Assert.Equal("NPU", StatusText.Device("NPU"));
        var device = StatusText.Describe(Status(device: "none"), T).Single(p => p.Key == "Status_Device");
        Assert.Equal(new LocalizedText("Status_DeviceNotLoaded"), device.Args[0]);
        Assert.True(Strings().ContainsKey("Status_DeviceNotLoaded"));
    }

    [Fact]
    public void Narrow_status_bar_drops_store_then_queue_then_audio()
    {
        var detail = PauseControl.DetailParts(Status(), T);
        Assert.Equal(["Status_AudioSystem", "Status_Device", "Status_Queue", "Status_Store"], detail.Select(p => p.Key));
        Assert.Equal(detail, StatusText.Fit(detail, 1200));
        Assert.Equal(["Status_AudioSystem", "Status_Device", "Status_Queue"], StatusText.Fit(detail, 900).Select(p => p.Key));
        Assert.Equal(["Status_AudioSystem", "Status_Device"], StatusText.Fit(detail, 760).Select(p => p.Key));
        Assert.Equal(["Status_Device"], StatusText.Fit(detail, 640).Select(p => p.Key));
    }

    // ---------------- D10: reconnect speed and the Start action

    [Fact]
    public void Status_reconnect_backoff_is_capped_near_5_s()
    {
        var sub = new StatusSubscription("unused", new ServerTrustVerifier(@"C:\nowhere\BetterRecall.Agent.exe"));
        Assert.True(sub.MaxBackoff <= TimeSpan.FromSeconds(5));
        Assert.True(sub.MinBackoff <= sub.MaxBackoff);
    }

    [Fact]
    public void Start_action_needs_an_installed_agent()
    {
        var root = Path.Combine(Path.GetTempPath(), "br-s3l-" + Guid.NewGuid().ToString("N"));
        var ui = Path.Combine(root, "ui");
        Directory.CreateDirectory(ui);
        try
        {
            Assert.Null(AgentStart.AgentPath(ui));
            Assert.False(AgentStart.TryStart(ui));
            var agent = Path.Combine(root, InstallLayout.AgentExe);
            File.WriteAllBytes(agent, []);
            Assert.Equal(agent, AgentStart.AgentPath(ui), ignoreCase: true);
            Assert.True(Strings().ContainsKey("Err_NotRunning_Start"));
        }
        finally { Directory.Delete(root, recursive: true); }
    }

    // ---------------- D15 and D14: narrow layouts

    [Fact]
    public void Settings_groups_move_to_the_top_when_narrow()
    {
        Assert.True(SettingsShellLayout.GroupsOnTop(852));    // 900 px window less the main icon rail
        Assert.True(SettingsShellLayout.GroupsOnTop(640));
        Assert.False(SettingsShellLayout.GroupsOnTop(1152));
        Assert.False(SettingsShellLayout.GroupsOnTop(0));     // before the first layout pass
    }

    [Fact]
    public void Audit_hides_results_and_pid_when_narrow()
    {
        Assert.True(AuditLayout.Narrow(564));
        Assert.False(AuditLayout.Narrow(900));
        var narrow = AuditLayout.FixedWidths(true);
        Assert.Equal(0, narrow[3]);
        Assert.Equal(0, narrow[4]);
        // time, pipe, verb, the 120 px client minimum and five 12 px gaps fit a 640 px window's page
        Assert.True(narrow.Sum() + 120 + 5 * 12 <= 564);
        Assert.Equal([150, 44, 120, 52, 60], AuditLayout.FixedWidths(false));
    }
}
