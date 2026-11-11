// SPDX-License-Identifier: MIT
// Copyright (c) BetterRecall contributors
using System.IO.Compression;
using System.Xml.Linq;
using BetterRecall.Contracts;
using BetterRecall.UI.Logic;
using BetterRecall.UI.Logic.Mock;

namespace BetterRecall.UI.Tests;

/// <summary>0.2 batch 2 in the UI: storage settings, storage pause, export.</summary>
public class StorageUiTests
{
    private static Dictionary<string, string> Strings()
    {
        var dir = new DirectoryInfo(AppContext.BaseDirectory);
        while (dir is not null && !File.Exists(Path.Combine(dir.FullName, "BetterRecall.slnx"))) dir = dir.Parent;
        Assert.NotNull(dir);
        var resw = XDocument.Load(Path.Combine(dir!.FullName, "src", "BetterRecall.UI", "Strings", "en-US", "Resources.resw"));
        return resw.Root!.Elements("data").ToDictionary(e => (string)e.Attribute("name")!, e => (string)e.Element("value")!);
    }

    [Fact]
    public void Storage_and_retention_keys_have_schema_entries_and_strings()
    {
        var s = Strings();
        Assert.Contains("storage", SettingsSchema.Groups);
        Assert.Null(SettingsSchema.Find("keyframes.maxGb"));
        foreach (var key in new[] { "storage.minFreeGb", "storage.maxGb", "storage.overwrite", "retention.progressiveAfterDays", "retention.progressiveOverDays" })
        {
            var d = SettingsSchema.Find(key);
            Assert.NotNull(d);
            Assert.True(s.ContainsKey(d!.LabelKey), d.LabelKey);
            Assert.True(s.ContainsKey(d.DescriptionKey), d.DescriptionKey);
        }
        Assert.Equal("W_storage_overwrite", SettingsSchema.Find("storage.overwrite")!.WarningKey);
        var format = SettingsSchema.Find("keyframes.format")!;
        Assert.Equal(["avif", "webp", "jxl", "jpeg"], format.Choices!);
        Assert.All(format.Choices!, c => Assert.True(s.ContainsKey(format.ChoiceKey(c)), c));
        foreach (var k in new[] { "G_storage", "G_storage_D", "Reason_storage", "StorageBar_Title", "StorageBar_Message", "StorageBar_Open",
                                  "Danger_ExportTitle.Text", "Danger_ExportWarnBody", "Danger_Exported", "Danger_ExportNone" })
            Assert.True(s.ContainsKey(k), k);
    }

    [Theory]
    [InlineData("storage.minFreeGb", -1, false)]
    [InlineData("storage.minFreeGb", 0, true)]
    [InlineData("storage.maxGb", 0, false)]
    [InlineData("storage.maxGb", 20000, true)]
    [InlineData("retention.progressiveAfterDays", 0, true)]
    [InlineData("retention.progressiveOverDays", 0, false)]
    public void Storage_ranges(string key, double value, bool valid)
    {
        var root = (System.Text.Json.Nodes.JsonObject)System.Text.Json.Nodes.JsonNode.Parse(
            System.Text.Json.JsonSerializer.Serialize(new Settings(), ContractsJson.Default.Settings))!;
        SettingsSchema.Set(root, key, value);
        Assert.Equal(valid, SettingsSchema.Validate(SettingsSchema.Find(key)!, root) is null);
    }

    [Fact]
    public void Storage_pause_shows_in_the_status_line()
    {
        var st = new MockAgent(new MockOptions(StoragePaused: true)).Status();
        Assert.True(st.Paused);
        Assert.Equal("storage", st.PauseReason);
        var parts = StatusText.Describe(st, DateTimeOffset.UtcNow);
        Assert.Equal("Status_Paused", parts[0].Key);
        Assert.Equal(AppArgs.Parse(["--mock-storage"]), new AppArgs(Mock: true, MockStorage: true));
    }

    [Fact]
    public async Task Mock_export_counts_then_writes_a_zip()
    {
        await using var h = new MockHarness();
        var to = DateTimeOffset.UtcNow;
        var from = to.AddDays(-2);
        var count = await h.Api.ExportAsync(new ExportRequest(from, to, "", DryRun: true));
        Assert.True(count.Ok, count.Error);
        Assert.True(count.Value!.Count > 0);
        Assert.True(count.Value.Images > 0);

        var path = Path.Combine(Path.GetTempPath(), "br-ui-export-" + Guid.NewGuid().ToString("N") + ".zip");
        try
        {
            var r = await h.Api.ExportAsync(new ExportRequest(from, to, path));
            Assert.True(r.Ok, r.Error);
            Assert.Equal(count.Value.Count, r.Value!.Count);
            using var zip = ZipFile.OpenRead(path);
            Assert.NotNull(zip.GetEntry("captures.json"));
            Assert.Contains(zip.Entries, e => e.FullName.StartsWith("images/", StringComparison.Ordinal));
        }
        finally { File.Delete(path); }

        Assert.False((await h.Api.ExportAsync(new ExportRequest(to, from, "", DryRun: true))).Ok);
        Assert.False((await h.Api.ExportAsync(new ExportRequest(from, to, "relative.zip"))).Ok);
    }

    [Fact]
    public async Task Mock_export_is_a_data_verb()
    {
        await using var h = new MockHarness(new MockOptions(StartLocked: true));
        var r = await h.Api.ExportAsync(new ExportRequest(DateTimeOffset.UtcNow.AddDays(-1), DateTimeOffset.UtcNow, "", DryRun: true));
        Assert.False(r.Ok);
        Assert.Equal(AgentErrorKind.Locked, r.Kind);
    }
}
