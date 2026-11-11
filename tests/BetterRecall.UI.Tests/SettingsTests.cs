// SPDX-License-Identifier: MIT
// Copyright (c) BetterRecall contributors
using System.Reflection;
using System.Text.Json;
using System.Text.Json.Nodes;
using BetterRecall.Contracts;
using BetterRecall.UI.Logic;

namespace BetterRecall.UI.Tests;

public class SettingsTests
{
    private static JsonObject Defaults() =>
        (JsonObject)JsonNode.Parse(JsonSerializer.Serialize(new Settings(), ContractsJson.Default.Settings))!;

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
        Assert.All(SettingsSchema.All, d => Assert.Contains(d.Group, SettingsSchema.Groups));
    }

    [Fact]
    public void Defaults_AreValid() => Assert.Empty(SettingsSchema.ValidateAll(Defaults()));

    [Fact]
    public void Diff_NoChange_IsEmpty()
    {
        Assert.Empty(SettingsDiff.Diff(Defaults(), Defaults()));
        Assert.Equal([], SettingsDiff.ToRequestLines(new JsonObject()));
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
        var lines = SettingsDiff.ToRequestLines(diff)!;
        Assert.Single(lines);
        Assert.StartsWith("settings set {", lines[0]);
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
    public void RequestLines_SplitPerSection_WhenTooLong()
    {
        var edited = Defaults();
        SettingsSchema.Set(edited, "privacy.denyProcesses", new JsonArray(Enumerable.Range(0, 160).Select(i => (JsonNode)$"process{i:D4}.exe").ToArray()));
        SettingsSchema.Set(edited, "ipc.allowClients", new JsonArray(Enumerable.Range(0, 60).Select(i => (JsonNode)$@"C:\Tools\client{i:D3}\c.exe").ToArray()));
        var lines = SettingsDiff.ToRequestLines(SettingsDiff.Diff(Defaults(), edited))!;
        Assert.Equal(2, lines.Count);
        Assert.All(lines, l => Assert.True(System.Text.Encoding.UTF8.GetByteCount(l) <= LineProtocol.MaxRequestBytes));
    }

    [Fact]
    public void RequestLines_NullWhenOneSectionIsTooLong()
    {
        var edited = Defaults();
        SettingsSchema.Set(edited, "privacy.denyProcesses", new JsonArray(Enumerable.Range(0, 400).Select(i => (JsonNode)$"process{i:D4}.exe").ToArray()));
        Assert.Null(SettingsDiff.ToRequestLines(SettingsDiff.Diff(Defaults(), edited)));
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
    [InlineData("keyframes.format", "png", false)]
    [InlineData("hotkeys.search", "Ctrl+Alt+R", false)]   // clashes with hotkeys.replay
    [InlineData("hotkeys.search", "Ctrl+Shift+F", true)]
    [InlineData("hotkeys.search", "Shift+F", false)]
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
        }
    }
}

public class SettingsEditorTests
{
    [Fact]
    public async Task Save_SendsOnlyChangedKeys_AndRebases()
    {
        await using var h = new MockHarness();
        var editor = new SettingsEditor();
        editor.Load((await h.Api.SettingsGetAsync()).Value!);
        editor.Set("capture.intervalMs", 3000);
        editor.Set("audio.mic", true);
        Assert.Equal(["audio.mic", "capture.intervalMs"], SettingsDiff.Keys(editor.Diff()).Order());

        var lines = new List<string>();
        var recorder = new RecordingClient(h.Client, lines);
        var (outcome, _) = await editor.SaveAsync(new AgentApi(recorder));
        Assert.Equal(SaveOutcome.Saved, outcome);
        Assert.Equal("settings set {\"capture\":{\"intervalMs\":3000},\"audio\":{\"mic\":true}}", Assert.Single(lines));
        Assert.False(editor.HasChanges);
        Assert.Equal(3000, SettingsSchema.Get(editor.Original, "capture.intervalMs")!.GetValue<int>());
        Assert.Equal((SaveOutcome.NothingToSave, (AgentResult<System.Text.Json.Nodes.JsonObject>?)null), await editor.SaveAsync(h.Api));
    }

    [Fact]
    public async Task SaveKey_SendsOneKey_KeepsOtherPendingEdits()
    {
        await using var h = new MockHarness();
        var editor = new SettingsEditor();
        editor.Load((await h.Api.SettingsGetAsync()).Value!);
        editor.Set("capture.intervalMs", 3000);
        editor.Set("audio.mic", true);   // replaced by the switch below

        var lines = new List<string>();
        var r = await editor.SaveKeyAsync(new AgentApi(new RecordingClient(h.Client, lines)), "audio.mic", false);
        Assert.True(r.Ok);
        Assert.Equal("settings set {\"audio\":{\"mic\":false}}", Assert.Single(lines));
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
        var lines = new List<string>();
        var (outcome, _) = await editor.SaveAsync(new AgentApi(new RecordingClient(h.Client, lines)));
        Assert.Equal(SaveOutcome.Invalid, outcome);
        Assert.Empty(lines);

        // a key the agent rejects but the local schema accepts
        var server = new FakeLineServer(l => l.StartsWith("settings set") ? Envelope.Error("Invalid audio.language.") : Envelope.Ok("null"));
        await using var _ = server;
        await using var client = new AgentClient(server.Name, MockHarness.OwnProcessTrust());
        editor.Set("capture.intervalMs", 2000);
        editor.Set("audio.language", "xx");
        var (o2, last) = await editor.SaveAsync(new AgentApi(client));
        Assert.Equal(SaveOutcome.Invalid, o2);
        Assert.Equal("Invalid audio.language.", editor.Errors["audio.language"]);
        Assert.True(editor.HasChanges);
    }

    private sealed class RecordingClient(IAgentClient inner, List<string> lines) : IAgentClient
    {
        public int ServerPid => inner.ServerPid;
        public Task<AgentResult> SendAsync(string line, CancellationToken ct = default)
        {
            if (line.StartsWith("settings set", StringComparison.Ordinal)) lines.Add(line);
            return inner.SendAsync(line, ct);
        }
    }
}
