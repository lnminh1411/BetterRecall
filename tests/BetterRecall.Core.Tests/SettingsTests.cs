// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Text.Json;
using BetterRecall.Contracts;

namespace BetterRecall.Core.Tests;

public class SettingsTests
{
    private static JsonElement J(string json) => JsonDocument.Parse(json).RootElement.Clone();

    [Fact]
    public void Missing_file_gives_defaults_and_writes_camelCase()
    {
        using var h = new TempHome(null);
        var s = SettingsStore.Load(h.Path);
        Assert.Equal(2000, s.Current.Capture.IntervalMs);
        var text = File.ReadAllText(DataPaths.SettingsFile(h.Path));
        Assert.Contains("\"intervalMs\": 2000", text);
        Assert.Contains("\"keyProtection\": \"auto\"", text);
        Assert.False(File.Exists(DataPaths.SettingsFile(h.Path) + ".tmp"));
    }

    [Fact]
    public void TryUpdate_merges_saves_and_raises_Changed()
    {
        using var h = new TempHome(null);
        var s = SettingsStore.Load(h.Path);
        Settings? seen = null;
        s.Changed += x => seen = x;
        Assert.Null(s.TryUpdate(J("{\"capture\":{\"intervalMs\":3000},\"ipc\":{\"enabled\":true}}")));
        Assert.Equal(3000, s.Current.Capture.IntervalMs);
        Assert.True(s.Current.Ipc.Enabled);
        Assert.Equal(1280, s.Current.Capture.MaxWidth); // untouched key kept
        Assert.NotNull(seen);
        var reloaded = SettingsStore.Load(h.Path);
        Assert.Equal(3000, reloaded.Current.Capture.IntervalMs);
    }

    [Theory]
    [InlineData("{\"capture\":{\"intervalMs\":499}}", "Invalid capture.intervalMs.")]
    [InlineData("{\"capture\":{\"intervalMs\":60001}}", "Invalid capture.intervalMs.")]
    [InlineData("{\"capture\":{\"intervalMs\":1.5}}", "Invalid capture.intervalMs.")]
    [InlineData("{\"capture\":{\"intervalMs\":\"2000\"}}", "Invalid capture.intervalMs.")]
    [InlineData("{\"capture\":{\"maxWidth\":639}}", "Invalid capture.maxWidth.")]
    [InlineData("{\"capture\":{\"dhashThreshold\":33}}", "Invalid capture.dhashThreshold.")]
    [InlineData("{\"capture\":{\"dirtyMinPercent\":100.5}}", "Invalid capture.dirtyMinPercent.")]
    [InlineData("{\"capture\":{\"monitors\":[-1]}}", "Invalid capture.monitors.")]
    [InlineData("{\"keyframes\":{\"format\":\"png\"}}", "Invalid keyframes.format.")]
    [InlineData("{\"keyframes\":{\"quality\":29}}", "Invalid keyframes.quality.")]
    [InlineData("{\"keyframes\":{\"maxGb\":0}}", "Invalid keyframes.maxGb.")]
    [InlineData("{\"keyframes\":{\"thumbCacheMb\":1025}}", "Invalid keyframes.thumbCacheMb.")]
    [InlineData("{\"replay\":{\"seconds\":14}}", "Invalid replay.seconds.")]
    [InlineData("{\"replay\":{\"fps\":61}}", "Invalid replay.fps.")]
    [InlineData("{\"replay\":{\"fps\":0}}", "Invalid replay.fps.")]
    [InlineData("{\"replay\":{\"bitrateKbps\":499}}", "Invalid replay.bitrateKbps.")]
    [InlineData("{\"replay\":{\"encoder\":\"x264\"}}", "Invalid replay.encoder.")]
    [InlineData("{\"replay\":{\"encoder\":\"av1_qsv\"}}", "Invalid replay.encoder.")]
    [InlineData("{\"replay\":{\"encoderAdapter\":\"\"}}", "Invalid replay.encoderAdapter.")]
    [InlineData("{\"replay\":{\"encoderAdapter\":\"   \"}}", "Invalid replay.encoderAdapter.")]
    [InlineData("{\"replay\":{\"layout\":\"grid\"}}", "Invalid replay.layout.")]
    [InlineData("{\"replay\":{\"layout\":\"permonitor\"}}", "Invalid replay.layout.")]
    [InlineData("{\"replay\":{\"resolution\":\"4k\"}}", "Invalid replay.resolution.")]
    [InlineData("{\"replay\":{\"scalePercent\":24}}", "Invalid replay.scalePercent.")]
    [InlineData("{\"replay\":{\"scalePercent\":101}}", "Invalid replay.scalePercent.")]
    [InlineData("{\"replay\":{\"exportDir\":\"relative\\\\dir\"}}", "Invalid replay.exportDir.")]
    [InlineData("{\"index\":{\"dim\":128}}", "Invalid index.dim.")]
    [InlineData("{\"retention\":{\"days\":0}}", "Invalid retention.days.")]
    [InlineData("{\"audio\":{\"language\":\"English\"}}", "Invalid audio.language.")]
    [InlineData("{\"audio\":{\"maxSegmentS\":31}}", "Invalid audio.maxSegmentS.")]
    [InlineData("{\"privacy\":{\"nsfwThreshold\":0}}", "Invalid privacy.nsfwThreshold.")]
    [InlineData("{\"privacy\":{\"nsfwThreshold\":1.0}}", "Invalid privacy.nsfwThreshold.")]
    [InlineData("{\"privacy\":{\"nsfwGrid\":4}}", "Invalid privacy.nsfwGrid.")]
    [InlineData("{\"privacy\":{\"nsfwGrid\":0}}", "Invalid privacy.nsfwGrid.")]
    [InlineData("{\"privacy\":{\"nsfwDetectorThreshold\":0}}", "Invalid privacy.nsfwDetectorThreshold.")]
    [InlineData("{\"privacy\":{\"nsfwDetectorThreshold\":1.0}}", "Invalid privacy.nsfwDetectorThreshold.")]
    [InlineData("{\"privacy\":{\"nsfwDetector\":\"yes\"}}", "Invalid privacy.nsfwDetector.")]
    [InlineData("{\"privacy\":{\"nsfwDevice\":\"tpu\"}}", "Invalid privacy.nsfwDevice.")]
    [InlineData("{\"privacy\":{\"nsfwDevice\":\"NPU\"}}", "Invalid privacy.nsfwDevice.")]
    [InlineData("{\"privacy\":{\"nsfwGpuAdapter\":\"\"}}", "Invalid privacy.nsfwGpuAdapter.")]
    [InlineData("{\"privacy\":{\"nsfwGpuAdapter\":\"   \"}}", "Invalid privacy.nsfwGpuAdapter.")]
    [InlineData("{\"inference\":{\"gpuAdapter\":\"\"}}", "Invalid inference.gpuAdapter.")]
    [InlineData("{\"inference\":{\"gpuAdapter\":\"   \"}}", "Invalid inference.gpuAdapter.")]
    [InlineData("{\"privacy\":{\"panicPurgeMinutes\":1441}}", "Invalid privacy.panicPurgeMinutes.")]
    [InlineData("{\"security\":{\"unlockTimeoutMin\":0}}", "Invalid security.unlockTimeoutMin.")]
    [InlineData("{\"capture\":{\"monitors\":[64]}}", "Invalid capture.monitors.")]
    [InlineData("{\"privacy\":{\"denyPathPrefixes\":[\"x\"]}}", "Invalid privacy.denyPathPrefixes.")]
    [InlineData("{\"inference\":{\"device\":\"tpu\"}}", "Invalid inference.device.")]
    [InlineData("{\"inference\":{\"imageTokens\":100}}", "Invalid inference.imageTokens.")]
    [InlineData("{\"inference\":{\"idleExitS\":9}}", "Invalid inference.idleExitS.")]
    [InlineData("{\"governance\":{\"deferCpuPercent\":9}}", "Invalid governance.deferCpuPercent.")]
    [InlineData("{\"security\":{\"keyProtection\":\"none\"}}", "Invalid security.keyProtection.")]
    [InlineData("{\"hotkeys\":{\"search\":\"Space\"}}", "Invalid hotkeys.search.")]
    [InlineData("{\"hotkeys\":{\"panic\":\"Ctrl+Hyper+X\"}}", "Invalid hotkeys.panic.")]
    [InlineData("{\"ipc\":{\"pipeName\":\"bad name\"}}", "Invalid ipc.pipeName.")]
    [InlineData("{\"ipc\":{\"maxResults\":201}}", "Invalid ipc.maxResults.")]
    [InlineData("{\"ipc\":{\"maxResults\":5}}", "Invalid ipc.maxResults.")]
    [InlineData("{\"ipc\":{\"defaultResults\":51}}", "Invalid ipc.defaultResults.")]
    [InlineData("{\"ipc\":{\"maxResponseBytes\":4095}}", "Invalid ipc.maxResponseBytes.")]
    [InlineData("{\"ipc\":{\"maxSearchPerMinute\":601}}", "Invalid ipc.maxSearchPerMinute.")]
    [InlineData("{\"ipc\":{\"maxPauseS\":59}}", "Invalid ipc.maxPauseS.")]
    [InlineData("{\"ipc\":{\"idleTimeoutS\":3601}}", "Invalid ipc.idleTimeoutS.")]
    [InlineData("{\"ipc\":{\"allowClients\":[\"notabs.exe\"]}}", "Invalid ipc.allowClients.")]
    [InlineData("{\"log\":{\"level\":\"trace\"}}", "Invalid log.level.")]
    [InlineData("{\"capture\":{\"nope\":1}}", "Invalid capture.nope.")]
    [InlineData("{\"nosection\":{}}", "Invalid nosection.")]
    [InlineData("{\"capture\":5}", "Invalid capture.")]
    [InlineData("[1]", "Invalid JSON.")]
    public void TryUpdate_rejects_bad_values_and_saves_nothing(string json, string error)
    {
        using var h = new TempHome(null);
        var s = SettingsStore.Load(h.Path);
        var before = File.ReadAllText(DataPaths.SettingsFile(h.Path));
        bool changed = false;
        s.Changed += _ => changed = true;
        Assert.Equal(error, s.TryUpdate(J(json)));
        Assert.False(changed);
        Assert.Equal(before, File.ReadAllText(DataPaths.SettingsFile(h.Path)));
    }

    [Theory]
    [InlineData("Ctrl+Alt+Space", true)]
    [InlineData("Ctrl+Alt+Shift+X", true)]
    [InlineData("Win+F12", true)]
    [InlineData("Ctrl+=", true)]
    [InlineData("Ctrl+Alt+\\", true)]
    [InlineData("Alt+NumPad5", true)]
    [InlineData("Ctrl+Enter", false)]
    [InlineData("Ctrl+F25", false)]
    [InlineData("Ctrl+", false)]
    [InlineData("Ctrl+Ctrl+X", false)]
    [InlineData("X", false)]
    [InlineData("", false)]
    public void Hotkey_format(string hk, bool ok) => Assert.Equal(ok, SettingsStore.ValidHotkey(hk));

    [Fact]
    public void Replay_keys_accept_spec_values()
    {
        using var h = new TempHome(null);
        var s = SettingsStore.Load(h.Path);
        Assert.Null(s.TryUpdate(JsonDocument.Parse("{\"replay\":{\"encoder\":\"hevc\",\"encoderAdapter\":\"Intel(R) Graphics\",\"layout\":\"perMonitor\",\"resolution\":\"scale\",\"scalePercent\":25,\"fps\":144}}").RootElement));
        Assert.Null(s.TryUpdate(JsonDocument.Parse("{\"replay\":{\"encoderAdapter\":\"" + new string('a', 128) + "\"}}").RootElement));
        Assert.Equal("Invalid replay.encoderAdapter.", s.TryUpdate(JsonDocument.Parse("{\"replay\":{\"encoderAdapter\":\"" + new string('a', 129) + "\"}}").RootElement));
    }

    [Fact]
    public void Nsfw_gate_keys_accept_spec_values()
    {
        using var h = new TempHome(null);
        var s = SettingsStore.Load(h.Path);
        Assert.Equal(1, s.Current.Privacy.NsfwGrid);
        Assert.True(s.Current.Privacy.NsfwDetector);
        Assert.Equal(0.4, s.Current.Privacy.NsfwDetectorThreshold);
        Assert.Equal("auto", s.Current.Privacy.NsfwDevice);
        Assert.Equal("auto", s.Current.Privacy.NsfwGpuAdapter);
        foreach (var d in new[] { "auto", "npu", "igpu", "gpu", "cpu" })
            Assert.Null(s.TryUpdate(JsonDocument.Parse("{\"privacy\":{\"nsfwDevice\":\"" + d + "\"}}").RootElement));
        Assert.Null(s.TryUpdate(JsonDocument.Parse("{\"privacy\":{\"nsfwDetector\":false,\"nsfwDetectorThreshold\":0.01,\"nsfwThreshold\":0.01,\"nsfwGpuAdapter\":\"RTX 3050\"}}").RootElement));
        Assert.Null(s.TryUpdate(JsonDocument.Parse("{\"privacy\":{\"nsfwDetectorThreshold\":0.99,\"nsfwThreshold\":0.99}}").RootElement));
        Assert.Equal("Invalid privacy.nsfwGpuAdapter.", s.TryUpdate(JsonDocument.Parse("{\"privacy\":{\"nsfwGpuAdapter\":\"" + new string('a', 129) + "\"}}").RootElement));
        Assert.Equal("RTX 3050", s.Current.Privacy.NsfwGpuAdapter);
    }

    [Fact]
    public void Inference_gpu_adapter_accepts_names_up_to_128_chars()
    {
        using var h = new TempHome(null);
        var s = SettingsStore.Load(h.Path);
        Assert.Equal("auto", s.Current.Inference.GpuAdapter);
        Assert.Null(s.TryUpdate(JsonDocument.Parse("{\"inference\":{\"device\":\"gpu\",\"gpuAdapter\":\"NVIDIA GeForce RTX 3050\"}}").RootElement));
        Assert.Null(s.TryUpdate(JsonDocument.Parse("{\"inference\":{\"gpuAdapter\":\"" + new string('a', 128) + "\"}}").RootElement));
        Assert.Equal("Invalid inference.gpuAdapter.", s.TryUpdate(JsonDocument.Parse("{\"inference\":{\"gpuAdapter\":\"" + new string('a', 129) + "\"}}").RootElement));
        Assert.Equal(new string('a', 128), s.Current.Inference.GpuAdapter);
        Assert.Equal("gpu", s.Current.Inference.Device);
    }

    [Fact]
    public void Ocr_and_redaction_settings_validate()
    {
        using var h = new TempHome(null);
        var s = SettingsStore.Load(h.Path);
        Assert.True(s.Current.Ocr.Enabled);
        Assert.Equal(("onnx", "auto", "auto"), (s.Current.Ocr.Engine, s.Current.Ocr.Languages, s.Current.Ocr.Device));
        var p = s.Current.Privacy;
        Assert.True(p.RedactPasswords && p.RedactCards && p.RedactIds && p.RedactBirthDates);
        Assert.False(p.RedactEmails || p.RedactPhones || p.RedactNames || p.RedactAddresses);

        Assert.Null(s.TryUpdate(JsonDocument.Parse("{\"ocr\":{\"engine\":\"windows\",\"languages\":\"en-US, ja,vi\",\"device\":\"cpu\"}}").RootElement));
        Assert.Null(s.TryUpdate(JsonDocument.Parse("{\"privacy\":{\"redactEmails\":true,\"redactNames\":true}}").RootElement));
        Assert.Equal("Invalid ocr.engine.", s.TryUpdate(JsonDocument.Parse("{\"ocr\":{\"engine\":\"tesseract\"}}").RootElement));
        Assert.Equal("Invalid ocr.languages.", s.TryUpdate(JsonDocument.Parse("{\"ocr\":{\"languages\":\"english\"}}").RootElement));
        Assert.Equal("Invalid ocr.languages.", s.TryUpdate(JsonDocument.Parse("{\"ocr\":{\"languages\":\"\"}}").RootElement));
        Assert.Equal("Invalid ocr.device.", s.TryUpdate(JsonDocument.Parse("{\"ocr\":{\"device\":\"npu\"}}").RootElement));
        Assert.Equal(("windows", "en-US, ja,vi", "cpu"), (s.Current.Ocr.Engine, s.Current.Ocr.Languages, s.Current.Ocr.Device));
        Assert.True(s.Current.Privacy.RedactEmails && s.Current.Privacy.RedactNames);
    }

    [Fact]
    public void Defaults_are_valid() => Assert.Null(SettingsStore.Validate(new Settings()));

    [Fact]
    public void Load_resets_bad_keys_and_keeps_good_ones()
    {
        using var h = new TempHome(null);
        File.WriteAllText(DataPaths.SettingsFile(h.Path), "{\"capture\":{\"intervalMs\":5,\"maxWidth\":1920},\"retention\":{\"days\":90}}");
        var s = SettingsStore.Load(h.Path);
        Assert.Equal(2000, s.Current.Capture.IntervalMs);
        Assert.Equal(1920, s.Current.Capture.MaxWidth);
        Assert.Equal(90, s.Current.Retention.Days);
    }

    [Fact]
    public void Corrupt_file_falls_back_to_defaults_and_keeps_a_copy()
    {
        using var h = new TempHome(null);
        File.WriteAllText(DataPaths.SettingsFile(h.Path), "{ not json");
        var s = SettingsStore.Load(h.Path);
        Assert.Equal(30, s.Current.Retention.Days);
        Assert.True(File.Exists(DataPaths.SettingsFile(h.Path) + ".bad"));
    }
}
