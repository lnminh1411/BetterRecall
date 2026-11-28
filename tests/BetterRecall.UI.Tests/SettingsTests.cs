// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Reflection;
using System.Text.Json;
using System.Text.Json.Nodes;
using BetterRecall.Contracts;
using BetterRecall.UI.Logic;

namespace BetterRecall.UI.Tests;

public class SettingsTests
{
    private static JsonObject Defaults() => TestResources.DefaultSettings();

    [Fact]
    public void Schema_CoversEveryContractsKey()
    {
        var keys = new List<string>();
        foreach (var section in typeof(Settings).GetProperties())
            foreach (var p in section.PropertyType.GetProperties(BindingFlags.Public | BindingFlags.Instance))
                keys.Add(JsonNamingPolicy.CamelCase.ConvertName(section.Name) + "." + JsonNamingPolicy.CamelCase.ConvertName(p.Name));
        var missing = keys.Where(k => SettingsSchema.Find(k) is null).ToList();
        Assert.Empty(missing);
        var extra = SettingsSchema.All.Select(d => d.Key).Except(keys).ToList();
        Assert.Empty(extra);
        Assert.All(SettingsSchema.All, d => Assert.NotNull(SettingsSchema.Get(Defaults(), d.Key)));
        Assert.All(SettingsSchema.All, d => Assert.Contains(d.Group, SettingsSchema.Groups.Append("hotkeys").Append("update")));   // hotkeys: Shortcuts page; update: Update page
        Assert.All(SettingsSchema.All.Where(d => d.Group == "hotkeys"), d => Assert.Contains(d.Key, ShortcutRules.Actions.Select(ShortcutRules.Key)));
    }

    [Fact]
    public void Schema_has_ai_security_and_storage_rules()
    {
        Assert.Equal(["hello", "password"], SettingsSchema.Find("security.unlockMethod")!.Choices!);
        Assert.Equal(["gemini", "openai-compatible", "claude"], SettingsSchema.Find("ai.provider")!.Choices!);
        Assert.Equal("ai.enabled", SettingsSchema.Find("ai.model")!.EnabledWhenKey);
        Assert.Equal(2, SettingsSchema.Find("replay.sampleSeconds")!.Min);
        Assert.Equal(60, SettingsSchema.Find("replay.sampleSeconds")!.Max);
        Assert.Null(SettingsSchema.Find("ai.apiKey"));
        Assert.Contains("ai", SettingsSchema.Groups);

        Assert.Contains("storage", SettingsSchema.Groups);
        Assert.Null(SettingsSchema.Find("keyframes.maxGb"));
        Assert.Null(SettingsSchema.Find("keyframes.encoder"));
        Assert.Equal("W_storage_overwrite", SettingsSchema.Find("storage.overwrite")!.WarningKey);
        Assert.Equal(["avif", "webp", "jxl", "jpeg"], SettingsSchema.Find("keyframes.format")!.Choices!);
        foreach (var key in new[] { "storage.minFreeGb", "storage.maxGb", "storage.overwrite", "retention.progressiveAfterDays", "retention.progressiveOverDays" })
            Assert.NotNull(SettingsSchema.Find(key));

        var root = JsonNode.Parse("{\"ai\":{\"enabled\":true,\"provider\":\"gemini\",\"endpoint\":\"https://x.example\",\"model\":\"bad model\"},\"replay\":{\"sampleSeconds\":5}}")!.AsObject();
        var errors = SettingsSchema.ValidateAll(root);
        Assert.Contains("ai.model", errors.Keys);
        Assert.DoesNotContain("ai.endpoint", errors.Keys);
    }

    [Fact]
    public void ScalePercent_EnabledOnlyForScale()
    {
        var s = Defaults();
        var d = SettingsSchema.Find("replay.scalePercent")!;
        Assert.False(d.IsEnabled(s));
        SettingsSchema.Set(s, "replay.resolution", "scale");
        Assert.True(d.IsEnabled(s));
        Assert.True(SettingsSchema.Find("replay.layout")!.IsEnabled(s));
    }

    [Fact]
    public void Defaults_AreValid() => Assert.Empty(SettingsSchema.ValidateAll(Defaults()));

    [Fact]
    public void Diff_NoChange_IsEmpty()
    {
        Assert.Empty(SettingsDiff.Diff(Defaults(), Defaults()));
        Assert.Equal("", SettingsDiff.ToRequestLine(new JsonObject()));
    }

    [Fact]
    public void Diff_OnlyChangedKeysAreSent()
    {
        var edited = Defaults();
        SettingsSchema.Set(edited, "capture.intervalMs", 3000);
        SettingsSchema.Set(edited, "security.unlockTimeoutMin", 5);
        SettingsSchema.Set(edited, "privacy.denyTitlePatterns", new JsonArray("InPrivate", "Incognito"));
        var diff = SettingsDiff.Diff(Defaults(), edited);
        Assert.Equal(["capture.intervalMs", "privacy.denyTitlePatterns", "security.unlockTimeoutMin"], SettingsDiff.Keys(diff).Order());
        Assert.Equal("{\"capture\":{\"intervalMs\":3000},\"privacy\":{\"denyTitlePatterns\":[\"InPrivate\",\"Incognito\"]},\"security\":{\"unlockTimeoutMin\":5}}",
            diff.ToJsonString());
        Assert.StartsWith("settings set {", SettingsDiff.ToRequestLine(diff));
    }

    [Fact]
    public void Diff_SettingBackToOriginal_IsNotSent()
    {
        var edited = Defaults();
        SettingsSchema.Set(edited, "capture.intervalMs", 3000);
        SettingsSchema.Set(edited, "capture.intervalMs", 2000);
        Assert.Empty(SettingsDiff.Diff(Defaults(), edited));
    }

    [Fact]
    public void Diff_DoubleVersusIntegerJson_IsNotAChange()
    {
        var original = (JsonObject)JsonNode.Parse("{\"privacy\":{\"nsfwThreshold\":0.8}}")!;
        var edited = (JsonObject)original.DeepClone();
        SettingsSchema.Set(edited, "privacy.nsfwThreshold", 0.8);
        Assert.Empty(SettingsDiff.Diff(original, edited));
    }

    [Fact]
    public void RequestLine_IsOneLine_WhenOver4KiB()
    {
        // B-checks-C S2: a save split per section stopped at the first error after earlier sections were saved.
        var edited = Defaults();
        SettingsSchema.Set(edited, "privacy.denyProcesses", new JsonArray(Enumerable.Range(0, 160).Select(i => (JsonNode)$"process{i:D4}.exe").ToArray()));
        SettingsSchema.Set(edited, "ipc.allowClients", new JsonArray(Enumerable.Range(0, 60).Select(i => (JsonNode)$@"C:\Tools\client{i:D3}\c.exe").ToArray()));
        var line = SettingsDiff.ToRequestLine(SettingsDiff.Diff(Defaults(), edited))!;
        Assert.True(System.Text.Encoding.UTF8.GetByteCount(line) > LineProtocol.MaxRequestBytes);
        Assert.True(System.Text.Encoding.UTF8.GetByteCount(line) <= LineProtocol.MaxUiRequestBytes);
        Assert.Contains("\"privacy\"", line);
        Assert.Contains("\"ipc\"", line);
    }

    [Fact]
    public void RequestLine_NullWhenOverTheUiPipeLimit()
    {
        var edited = Defaults();
        SettingsSchema.Set(edited, "privacy.denyProcesses", new JsonArray(Enumerable.Range(0, 5000).Select(i => (JsonNode)$"process{i:D4}.exe").ToArray()));
        Assert.Null(SettingsDiff.ToRequestLine(SettingsDiff.Diff(Defaults(), edited)));
    }

    [Fact]
    public void RequestLine_NullWhenSectionsFitAloneButNotTogether()
    {
        // H-core C2 cap + B-checks-C S2: two sections that each fit 64 KiB but not together are refused, never split.
        var edited = Defaults();
        SettingsSchema.Set(edited, "privacy.denyProcesses", new JsonArray(Enumerable.Range(0, 2600).Select(i => (JsonNode)$"process{i:D4}.exe").ToArray()));
        SettingsSchema.Set(edited, "ipc.allowClients", new JsonArray(Enumerable.Range(0, 1000).Select(i => (JsonNode)$@"C:\Tools\client{i:D4}\c.exe").ToArray()));
        Assert.Null(SettingsDiff.ToRequestLine(SettingsDiff.Diff(Defaults(), edited)));
    }

    /// <summary>D18: pending change count has singular and plural resource keys.</summary>
    [Fact]
    public void Pending_count_has_a_singular()
    {
        var s = TestResources.Strings();
        Assert.Equal("Settings_PendingOne", SettingsDiff.PendingKey(1));
        Assert.Equal("Settings_Pending", SettingsDiff.PendingKey(2));
        Assert.Equal("1 unsaved change", s[SettingsDiff.PendingKey(1)]);
        Assert.Contains("{0}", s[SettingsDiff.PendingKey(3)]);
    }

    [Theory]
    [InlineData("capture.intervalMs", 499, false)]
    [InlineData("capture.intervalMs", 500, true)]
    [InlineData("capture.intervalMs", 60001, false)]
    [InlineData("privacy.nsfwThreshold", 0, false)]
    [InlineData("privacy.nsfwThreshold", 1.0, false)]
    [InlineData("privacy.nsfwThreshold", 0.01, true)]
    [InlineData("privacy.nsfwThreshold", 0.95, true)]
    [InlineData("privacy.nsfwDetectorThreshold", 0, false)]
    [InlineData("privacy.nsfwDetectorThreshold", 1.0, false)]
    [InlineData("privacy.nsfwDetectorThreshold", 0.01, true)]
    [InlineData("privacy.nsfwDetectorThreshold", 0.99, true)]
    [InlineData("retention.days", 3650, true)]
    [InlineData("retention.days", 0, false)]
    [InlineData("index.dim", 512, true)]
    [InlineData("index.dim", 128, false)]
    [InlineData("ipc.defaultResults", 60, false)]   // above ipc.maxResults (50)
    [InlineData("replay.scalePercent", 25, true)]
    [InlineData("replay.scalePercent", 24, false)]
    [InlineData("replay.scalePercent", 101, false)]
    [InlineData("storage.minFreeGb", -1, false)]
    [InlineData("storage.minFreeGb", 0, true)]
    [InlineData("storage.maxGb", 0, false)]
    [InlineData("storage.maxGb", 20000, true)]
    [InlineData("retention.progressiveAfterDays", 0, true)]
    [InlineData("retention.progressiveOverDays", 0, false)]
    public void Validate_Ranges(string key, double value, bool valid)
    {
        var s = Defaults();
        SettingsSchema.Set(s, key, value == Math.Floor(value) ? JsonValue.Create((int)value) : JsonValue.Create(value));
        var err = SettingsSchema.Validate(SettingsSchema.Find(key)!, s);
        Assert.Equal(valid, err is null);
        if (!valid) Assert.Equal($"Invalid {key}.", err);
    }

    [Theory]
    [InlineData("ipc.pipeName", "BetterRecall", true)]
    [InlineData("ipc.pipeName", "bad name", false)]
    [InlineData("audio.language", "auto", true)]
    [InlineData("audio.language", "vi", true)]
    [InlineData("audio.language", "Vietnamese", false)]
    [InlineData("replay.exportDir", "", true)]
    [InlineData("replay.exportDir", @"D:\Clips", true)]
    [InlineData("replay.exportDir", @"relative\dir", false)]
    [InlineData("replay.encoder", "av1", true)]
    [InlineData("replay.encoder", "av1_qsv", false)]
    [InlineData("replay.layout", "perMonitor", true)]
    [InlineData("replay.layout", "grid", false)]
    [InlineData("replay.resolution", "900p", true)]
    [InlineData("replay.resolution", "800p", false)]
    [InlineData("replay.encoderAdapter", "RTX 3050", true)]
    [InlineData("replay.encoderAdapter", " ", false)]
    [InlineData("keyframes.format", "png", false)]
    [InlineData("hotkeys.search", "Ctrl+Alt+R", false)]   // clashes with hotkeys.replay
    [InlineData("hotkeys.search", "Ctrl+Shift+F", true)]
    [InlineData("hotkeys.search", "Shift+F", false)]
    [InlineData("hotkeys.panic", "", true)]                // not assigned
    [InlineData("hotkeys.lock", "Ctrl+Alt+L", true)]
    [InlineData("hotkeys.lock", "Ctrl+Alt+P", false)]     // clashes with hotkeys.pause
    public void Validate_Strings(string key, string value, bool valid)
    {
        var s = Defaults();
        SettingsSchema.Set(s, key, value);
        Assert.Equal(valid, SettingsSchema.Validate(SettingsSchema.Find(key)!, s) is null);
    }

    [Fact]
    public void Validate_AllowClientsNeedsAbsolutePaths()
    {
        var s = Defaults();
        SettingsSchema.Set(s, "ipc.allowClients", new JsonArray("tool.exe"));
        Assert.NotNull(SettingsSchema.Validate(SettingsSchema.Find("ipc.allowClients")!, s));
        SettingsSchema.Set(s, "ipc.allowClients", new JsonArray(@"C:\Tools\tool.exe"));
        Assert.Null(SettingsSchema.Validate(SettingsSchema.Find("ipc.allowClients")!, s));
    }

    [Theory]
    [InlineData("ctrl+alt+space", "Ctrl+Alt+Space")]
    [InlineData("Alt+Ctrl+Shift+x", "Ctrl+Alt+Shift+X")]
    [InlineData("Win+F12", "Win+F12")]
    [InlineData("Ctrl+NumPad5", "Ctrl+NumPad5")]
    [InlineData("Shift+A", null)]
    [InlineData("Ctrl+Ctrl+A", null)]
    [InlineData("Ctrl+", null)]
    [InlineData("A", null)]
    [InlineData("Ctrl+Hyper", null)]
    public void Hotkey_Normalize(string input, string? expected) => Assert.Equal(expected, HotkeyFormat.Normalize(input));

    [Fact]
    public void Hotkey_FromKeyPress()
    {
        Assert.Equal("Ctrl+Alt+Space", HotkeyFormat.FromKeyPress(0x20, true, true, false, false));
        Assert.Equal("Ctrl+Shift+F5", HotkeyFormat.FromKeyPress(0x74, true, false, true, false));
        Assert.Null(HotkeyFormat.FromKeyPress(0x41, false, false, true, false));   // Shift alone
        Assert.Null(HotkeyFormat.FromKeyPress(0x11, true, false, false, false));   // the Ctrl key itself
        foreach (var vk in new[] { 0x41, 0x5A, 0x30, 0x70, 0x87, 0x60, 0xBA, 0x2E })
        {
            var hk = HotkeyFormat.FromKeyPress(vk, true, false, false, false)!;
            Assert.Equal(hk, HotkeyFormat.Normalize(hk));
            Assert.Equal(vk, HotkeyFormat.VirtualKey(hk.Split('+')[^1]));
        }
    }

    /// <summary>D24: Tab leaves the hotkey recorder unless a modifier is held.</summary>
    [Fact]
    public void Tab_leaves_the_hotkey_recorder_unless_a_modifier_is_down()
    {
        Assert.True(HotkeyFormat.LeavesRecorder(9, false, false, false));
        Assert.False(HotkeyFormat.LeavesRecorder(9, true, false, false));
        Assert.False(HotkeyFormat.LeavesRecorder(9, false, true, false));
        Assert.False(HotkeyFormat.LeavesRecorder(9, false, false, true));
        Assert.False(HotkeyFormat.LeavesRecorder(0x41, false, false, false));
    }

    [Fact]
    public void Shortcuts_ConflictsAndParse()
    {
        var s = Defaults();
        Assert.Equal("pause", ShortcutRules.UsedBy(s, "lock", "ctrl+alt+p"));
        Assert.Null(ShortcutRules.UsedBy(s, "pause", "Ctrl+Alt+P"));      // its own combination
        Assert.Null(ShortcutRules.UsedBy(s, "lock", ""));
        Assert.Null(ShortcutRules.UsedBy(s, "lock", "Ctrl+Alt+L"));
        Assert.Equal("", ShortcutRules.Get(s, "timeline"));
        Assert.True(ShortcutRules.TryParse("Ctrl+Alt+Shift+X", out var mods, out var vk));
        Assert.Equal(2u | 1u | 4u, mods);
        Assert.Equal(0x58u, vk);
        Assert.True(ShortcutRules.TryParse("Win+F12", out mods, out vk));
        Assert.Equal(8u, mods);
        Assert.Equal(0x7Bu, vk);
        Assert.False(ShortcutRules.TryParse("Shift+X", out _, out _));
    }

    /// <summary>D7: Shortcuts view signature ignores status noise and order of failed actions.</summary>
    [Fact]
    public void Shortcuts_signature_ignores_status_noise()
    {
        var settings = new JsonObject { ["hotkeys"] = new JsonObject { ["search"] = "Ctrl+Alt+Space", ["pause"] = "" } };
        var a = ShortcutRules.ViewSignature(settings, true, []);
        Assert.Equal(a, ShortcutRules.ViewSignature(settings, true, null));
        Assert.NotEqual(a, ShortcutRules.ViewSignature(settings, false, []));
        Assert.NotEqual(a, ShortcutRules.ViewSignature(settings, true, ["search"]));
        Assert.Equal(ShortcutRules.ViewSignature(settings, true, ["search", "pause"]), ShortcutRules.ViewSignature(settings, true, ["pause", "search"]));
        var changed = (JsonObject)settings.DeepClone();
        SettingsSchema.Set(changed, "hotkeys.pause", "Ctrl+Alt+P");
        Assert.NotEqual(a, ShortcutRules.ViewSignature(changed, true, []));
    }
}

public class SettingsEditorTests
{
    private static ScriptedClient RecordingSettingsSet(IAgentClient inner) =>
        ScriptedClient.Recording(inner, l => l.StartsWith("settings set", StringComparison.Ordinal));

    [Fact]
    public async Task Save_SendsOnlyChangedKeys_AndRebases()
    {
        await using var h = new MockHarness();
        var editor = new SettingsEditor();
        editor.Load((await h.Api.SettingsGetAsync()).Value!);
        editor.Set("capture.intervalMs", 3000);
        editor.Set("audio.mic", true);
        Assert.Equal(["audio.mic", "capture.intervalMs"], SettingsDiff.Keys(editor.Diff()).Order());

        var recorder = RecordingSettingsSet(h.Client);
        var (outcome, _) = await editor.SaveAsync(new AgentApi(recorder));
        Assert.Equal(SaveOutcome.Saved, outcome);
        Assert.Equal("settings set {\"capture\":{\"intervalMs\":3000},\"audio\":{\"mic\":true}}", Assert.Single(recorder.Lines));
        Assert.False(editor.HasChanges);
        Assert.Equal(3000, SettingsSchema.Get(editor.Original, "capture.intervalMs")!.GetValue<int>());
        Assert.Equal((SaveOutcome.NothingToSave, (AgentResult<JsonObject>?)null), await editor.SaveAsync(h.Api));
    }

    [Fact]
    public async Task Save_IgnoresAKeyTheAgentAcceptedButTheUiRulesReject()
    {
        await using var h = new MockHarness();
        var editor = new SettingsEditor();
        var loaded = (await h.Api.SettingsGetAsync()).Value!;
        loaded["ipc"]!["pipeName"] = @"\\.\pipe\MyScripts";   // Core strips the prefix; the UI pattern does not allow it
        Assert.True(SettingsSchema.ValidateAll(loaded).ContainsKey("ipc.pipeName"));
        editor.Load(loaded);
        editor.Set("capture.intervalMs", 3000);
        var recorder = RecordingSettingsSet(h.Client);
        var (outcome, _) = await editor.SaveAsync(new AgentApi(recorder));
        Assert.Equal(SaveOutcome.Saved, outcome);
        Assert.Equal("settings set {\"capture\":{\"intervalMs\":3000}}", Assert.Single(recorder.Lines));
    }

    [Fact]
    public async Task SaveKey_SendsOneKey_KeepsOtherPendingEdits()
    {
        await using var h = new MockHarness();
        var editor = new SettingsEditor();
        editor.Load((await h.Api.SettingsGetAsync()).Value!);
        editor.Set("capture.intervalMs", 3000);
        editor.Set("audio.mic", true);   // replaced by the switch below

        var recorder = RecordingSettingsSet(h.Client);
        var r = await editor.SaveKeyAsync(new AgentApi(recorder), "audio.mic", false);
        Assert.True(r.Ok);
        Assert.Equal("settings set {\"audio\":{\"mic\":false}}", Assert.Single(recorder.Lines));
        Assert.False(SettingsSchema.Get(editor.Original, "audio.mic")!.GetValue<bool>());
        Assert.Equal(["capture.intervalMs"], SettingsDiff.Keys(editor.Diff()));
    }

    [Fact]
    public async Task Save_LocalErrorBlocksSend_ServerErrorIsKeptInline()
    {
        await using var h = new MockHarness();
        var editor = new SettingsEditor();
        editor.Load((await h.Api.SettingsGetAsync()).Value!);
        editor.Set("capture.intervalMs", 10);
        Assert.True(editor.Errors.ContainsKey("capture.intervalMs"));
        var recorder = RecordingSettingsSet(h.Client);
        var (outcome, _) = await editor.SaveAsync(new AgentApi(recorder));
        Assert.Equal(SaveOutcome.Invalid, outcome);
        Assert.Empty(recorder.Lines);

        // A key the agent rejects but the local schema accepts.
        var server = new FakeLineServer(l => l.StartsWith("settings set") ? Envelope.Error("Invalid audio.language.") : Envelope.Ok("null"));
        await using var _ = server;
        await using var client = new AgentClient(server.Name, MockHarness.OwnProcessTrust());
        editor.Set("capture.intervalMs", 2000);
        editor.Set("audio.language", "xx");
        var (o2, _) = await editor.SaveAsync(new AgentApi(client));
        Assert.Equal(SaveOutcome.Invalid, o2);
        Assert.Equal("Invalid audio.language.", editor.Errors["audio.language"]);
        Assert.True(editor.HasChanges);
    }

    /// <summary>D2: settings fetched after a reconnect become the new baseline; pending edits stay on top.</summary>
    [Fact]
    public async Task Refresh_rebases_and_keeps_pending_edits()
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

    /// <summary>D2: Refresh before Load acts as initial Load.</summary>
    [Fact]
    public void Refresh_before_load_loads()
    {
        var editor = new SettingsEditor();
        editor.Refresh(new JsonObject { ["capture"] = new JsonObject { ["intervalMs"] = 5000 } });
        Assert.True(editor.Loaded);
        Assert.False(editor.HasChanges);
    }

    /// <summary>B8/D2: settings reload on first connect if not yet loaded, and on every reconnect.</summary>
    [Theory]
    [InlineData(1, false, true)]    // UI started before the agent: InitAsync got nothing (B8)
    [InlineData(1, true, false)]    // InitAsync already loaded them
    [InlineData(2, true, true)]     // reconnect: the agent may have restarted (D2)
    public void Settings_reload_on_connect(int connects, bool loaded, bool expected) =>
        Assert.Equal(expected, SettingsReload.OnConnect(connects, loaded));

    /// <summary>B4 (D3): inline passphrase prompt verifies and retries save, or stops on cancel.</summary>
    [Fact]
    public async Task Settings_save_flow_verifies_passphrase_inline_and_stops_on_cancel()
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
        await using var client = new AgentClient(srv.Name, MockHarness.OwnProcessTrust());
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
}
