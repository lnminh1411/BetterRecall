// SPDX-License-Identifier: MIT
// Copyright (c) BetterRecall contributors
using System.Text.Json;
using System.Text.Json.Nodes;
using System.Xml.Linq;
using BetterRecall.Contracts;
using BetterRecall.UI.Logic;

namespace BetterRecall.UI.Tests;

/// <summary>inference.gpuAdapter in the settings schema, its strings, the adapter list and the per-GPU bench rows.</summary>
public class GpuAdapterTests
{
    private static JsonObject Defaults() =>
        (JsonObject)JsonNode.Parse(JsonSerializer.Serialize(new Settings(), ContractsJson.Default.Settings))!;

    [Fact]
    public void Schema_entry_follows_inference_device_and_is_gated_on_gpu()
    {
        var all = SettingsSchema.All.ToList();
        int i = all.FindIndex(d => d.Key == "inference.gpuAdapter");
        Assert.True(i > 0);
        Assert.Equal("inference.device", all[i - 1].Key);
        var d = all[i];
        Assert.Equal(("inference", SettingType.Adapter, "inference.device", "gpu"), (d.Group, d.Type, d.EnabledWhenKey, d.EnabledWhenValue));

        var s = Defaults();
        Assert.Equal("auto", SettingsSchema.Get(s, "inference.gpuAdapter")!.GetValue<string>());
        Assert.False(d.IsEnabled(s));
        SettingsSchema.Set(s, "inference.device", "gpu");
        Assert.True(d.IsEnabled(s));
        SettingsSchema.Set(s, "inference.gpuAdapter", "RTX 3050");
        Assert.Null(SettingsSchema.Validate(d, s));
        SettingsSchema.Set(s, "inference.gpuAdapter", "  ");
        Assert.Equal("Invalid inference.gpuAdapter.", SettingsSchema.Validate(d, s));
    }

    private static Dictionary<string, string> Strings()
    {
        var dir = new DirectoryInfo(AppContext.BaseDirectory);
        while (dir is not null && !File.Exists(Path.Combine(dir.FullName, "BetterRecall.slnx"))) dir = dir.Parent;
        Assert.NotNull(dir);
        var resw = XDocument.Load(Path.Combine(dir!.FullName, "src", "BetterRecall.UI", "Strings", "en-US", "Resources.resw"));
        return resw.Root!.Elements("data").ToDictionary(e => (string)e.Attribute("name")!, e => (string)e.Element("value")!);
    }

    [Fact]
    public void Strings_exist_for_the_setting_its_auto_label_and_the_reason()
    {
        var s = Strings();
        var d = SettingsSchema.Find("inference.gpuAdapter")!;
        Assert.Equal("Graphics card", s[d.LabelKey]);
        Assert.Equal("Which GPU to use when Device is GPU. Automatic uses the integrated GPU through OpenVINO when there is one, else the first graphics card.",
            s[d.DescriptionKey]);
        Assert.Equal("Automatic", s[d.ChoiceKey("auto")]);
        Assert.Equal("NVIDIA GeForce RTX 3050 is the fastest graphics card here: 80 ms per image.",
            TextFormat.Fill(s["R_gpu_fastest"], ["NVIDIA GeForce RTX 3050", "80"]));
        // Every Adapter setting has its auto label.
        Assert.All(SettingsSchema.All.Where(x => x.Type == SettingType.Adapter && x.EnabledWhenKey is not null), x => Assert.True(s.ContainsKey(x.ChoiceKey("auto")), x.Key));
    }

    [Fact]
    public void Adapter_choices_keep_integrated_and_hardware_cards_but_not_basic_render()
    {
        GpuInfo G(string name, string vendor, bool integrated = false) => new(name, vendor, integrated, 0, "0x1", "1", []);
        var names = SettingsSchema.AdapterChoices(
        [
            G("AMD Radeon RX 6600", "AMD"), G("NVIDIA GeForce RTX 3050", "NVIDIA"), G("Intel(R) Arc(TM) Graphics", "Intel", true),
            G("Microsoft Basic Render Driver", "0x1414"), G("NVIDIA GeForce RTX 3050", "NVIDIA"), G(" ", "AMD"),
        ]);
        Assert.Equal(["AMD Radeon RX 6600", "NVIDIA GeForce RTX 3050", "Intel(R) Arc(TM) Graphics"], names);
        Assert.Empty(SettingsSchema.AdapterChoices(null));
    }

    [Fact]
    public void Inference_rows_name_each_gpu_and_mark_only_the_chosen_one()
    {
        var r = new BenchResult("GPU",
        [
            new BenchDevice("CPU", "OpenVINO", true, 900, 30, 1, null),
            new BenchDevice("GPU", "OpenVINO", true, 300, 8, 0.9996, null, Adapter: "Intel(R) Arc(TM) Graphics (iGPU)", AdapterAuto: true),
            new BenchDevice("GPU", "DirectML", true, 80, 6, 0.9997, null, Adapter: "NVIDIA GeForce RTX 3050"),
        ],
        [new TokenBench("GPU", 70, 50), new TokenBench("GPU", 140, 80), new TokenBench("GPU", 280, 150)],
        ChosenAdapter: "NVIDIA GeForce RTX 3050");
        var rows = BenchViewModel.InferenceRows(r);
        Assert.Equal(["CPU", "Intel(R) Arc(TM) Graphics (iGPU)", "NVIDIA GeForce RTX 3050"], rows.Select(x => x.Device));
        Assert.Equal([false, false, true], rows.Select(x => x.Chosen));
        Assert.Equal((50.0, 80.0, 150.0), (rows[2].Ms70!.Value, rows[2].Ms140!.Value, rows[2].Ms280!.Value));
        Assert.Equal((null, 300.0, null), (rows[1].Ms70, rows[1].Ms140, rows[1].Ms280));   // measured at the configured budget only

        // Single GPU (no adapter names): unchanged.
        var single = BenchViewModel.InferenceRows(new BenchResult("GPU", [new BenchDevice("GPU", "DirectML", true, 40, 5, 0.9998, null)], [new TokenBench("GPU", 70, 30)]));
        Assert.Equal(("GPU", true, 30.0), (single[0].Device, single[0].Chosen, single[0].Ms70!.Value));
    }
}
