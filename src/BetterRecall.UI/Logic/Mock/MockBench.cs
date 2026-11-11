// SPDX-License-Identifier: MIT
// Copyright (c) BetterRecall contributors
using System.Globalization;
using System.Text.Json.Nodes;
using BetterRecall.Contracts;

namespace BetterRecall.UI.Logic.Mock;

/// <summary>
/// A plausible BenchReport for the dev PC: Core Ultra 7 265K (NPU, Arc iGPU), RX 6600, RTX 3050, three monitors.
/// Numbers are made up but follow the section 10 rules, so the recommendations are consistent with them.
/// </summary>
public static class MockBench
{
    private const string Arc = "Intel(R) Arc(TM) Graphics";
    private const string Rx = "AMD Radeon RX 6600";
    private const string Rtx = "NVIDIA GeForce RTX 3050";

    public static readonly HardwareInfo Hardware = new(
        "Intel(R) Core(TM) Ultra 7 265K", 20, 20, 32_768, false,
        [
            new GpuInfo(Arc, "Intel", true, 128, "0x0000D3A1", "32.0.101.8132", ["av1", "hevc", "h264"]),
            new GpuInfo(Rx, "AMD", false, 8_176, "0x00011F42", "32.0.21025.1024", ["hevc", "h264"]),
            new GpuInfo(Rtx, "NVIDIA", false, 8_192, "0x00012A07", "581.57", ["hevc", "h264"]),
        ],
        new NpuInfo("Intel(R) AI Boost", "32.0.100.4297"),
        [
            new MonitorInfo(0, 2560, 1440, 165, Rx),
            new MonitorInfo(1, 1920, 1080, 60, Rx),
            new MonitorInfo(2, 1920, 1080, 75, Rtx),
        ],
        "Windows 11 Pro 10.0.29704");

    public static readonly BenchResult Inference = new("NPU",
    [
        new BenchDevice("NPU", "OpenVINO", true, 388.4, 14.1, 0.9996, null, "embeddinggemma-2", null, 41_200),
        new BenchDevice("GPU", "OpenVINO", true, 94.7, 6.2, 0.9999, null, "embeddinggemma-2", null, 18_900),
        new BenchDevice("CPU", "OpenVINO", true, 1_212.0, 37.5, 1.0, null, "embeddinggemma-2"),
    ],
    [
        new TokenBench("NPU", 70, 206.3), new TokenBench("NPU", 140, 388.4), new TokenBench("NPU", 280, 771.9),
        new TokenBench("GPU", 70, 51.8), new TokenBench("GPU", 140, 94.7), new TokenBench("GPU", 280, 181.0),
        new TokenBench("CPU", 70, 640.2), new TokenBench("CPU", 140, 1_212.0), new TokenBench("CPU", 280, 2_377.5),
    ]);

    /// <summary>P95 encode ms per megapixel, by adapter and codec. Null: no hardware encoder for that codec.</summary>
    private static double? CostPerMp(string adapter, string codec) => (adapter, codec) switch
    {
        (Arc, "av1") => 0.75, (Arc, "hevc") => 0.65, (Arc, "h264") => 0.55,
        (Rx, "hevc") => 0.90, (Rx, "h264") => 0.70,
        (Rtx, "hevc") => 0.80, (Rtx, "h264") => 0.60,
        _ => null,
    };

    public static EncoderBench[] Encoders()
    {
        var list = new List<EncoderBench>();
        (string Layout, int W, int H, double Mp)[] sizes =
        [
            ("composited", 6400, 1440, 9.22), ("composited", 4800, 1080, 5.18),
            ("perMonitor", 2560, 1440, 8.83), ("perMonitor", 1920, 1080, 4.97),   // Mp: all monitors' pixels encoded per frame
        ];
        foreach (var adapter in new[] { Arc, Rx, Rtx })
            foreach (var codec in new[] { "av1", "hevc", "h264" })
                foreach (var (layout, w, h, mp) in sizes)
                    foreach (var fps in new[] { 30, 60, 165 })
                    {
                        var api = adapter == Arc && codec == "av1" ? "vpl" : "mf";
                        var cost = CostPerMp(adapter, codec);
                        if (cost is null)
                        {
                            list.Add(new EncoderBench(adapter, codec, api, layout, w, h, fps, false, 0, 0, 0, "No hardware AV1 encoder on this adapter."));
                            continue;
                        }
                        double p95 = Math.Round(mp * cost.Value * (layout == "perMonitor" ? 1.15 : 1.0) * (1 + fps / 1000.0), 1);
                        double cpu = Math.Round(layout == "composited" ? 0.3 + fps * 0.006 : 0.5 + fps * 0.012, 2);
                        long mb = (long)(layout == "composited" ? 70 + fps * 0.5 : 110 + fps * 0.9);
                        string? error = null;
                        if (adapter == Rtx && layout == "perMonitor" && fps == 165 && w == 2560)
                            error = "Encoder session limit reached (MF_E_HW_MFT_FAILED_START_STREAMING).";
                        else if (adapter == Rx && codec == "hevc" && layout == "composited" && w == 6400 && fps == 165)
                            error = "Encoder fell behind and was stopped.";
                        list.Add(error is null
                            ? new EncoderBench(adapter, codec, api, layout, w, h, fps, true, p95, cpu, mb, null)
                            : new EncoderBench(adapter, codec, api, layout, w, h, fps, false, 0, 0, 0, error));
                    }
        return [.. list];
    }

    public static BenchReport Build(JsonObject settings, DateTimeOffset ts, long rows)
    {
        var recs = new List<Recommendation>();
        void Rec(string key, JsonNode value, string reason, params string[] args)
        {
            var current = SettingsSchema.Get(settings, key)?.ToJsonString() ?? "null";
            var proposed = value.ToJsonString();
            if (current != proposed) recs.Add(new Recommendation(key, proposed, current, reason, args));
        }
        string F(double v) => v.ToString("0.#", CultureInfo.InvariantCulture);

        // image tokens: 3 monitors x 0.3 / interval
        double interval = SettingsSchema.TryNumber(SettingsSchema.Get(settings, "capture.intervalMs"), out var iv) ? iv : 2000;
        double kps = 3 * 0.3 / (interval / 1000);
        foreach (var t in new[] { 280, 140, 70 })
        {
            var ms = Inference.Tokens!.First(x => x.Device == "NPU" && x.ImageTokens == t).ImageMs;
            if (ms / 1000 * kps <= 0.5)
            {
                Rec("inference.imageTokens", t, "R_tokens_budget", t.ToString(CultureInfo.InvariantCulture), F(ms), F(Math.Round(kps, 3)), F(Math.Round(ms / 1000 * kps * 100, 1)));
                break;
            }
        }
        // encoder: AV1 on the iGPU at native composited 60 fps: 6.9 ms P95 < 8.3 ms
        var av1 = Encoders().First(e => e.Adapter == Arc && e.Codec == "av1" && e.Layout == "composited" && e.Width == 6400 && e.Fps == 60);
        Rec("replay.encoder", "av1", "R_encoder_realtime", "AV1", Arc, F(av1.EncodeMsP95), F(1000.0 / 60));
        Rec("replay.encoderAdapter", Arc, "R_encoder_realtime", "AV1", Arc, F(av1.EncodeMsP95), F(1000.0 / 60));
        Rec("replay.fps", 60, "R_fps_headroom", "60", F(av1.EncodeMsP95), F(Math.Round(1000.0 / 60, 1)), F(av1.CpuPercent));
        Rec("replay.bitrateKbps", 19500, "R_bitrate_quality", "19500", "6400", "1440", "60", "0.035");

        return new BenchReport(ts, 214_000, Hardware, Inference, Encoders(), new CaptureBench(0.8, 6.4, 3.1, "oneVPL AV1 VDENC on Intel(R) Graphics"),
            new SearchBench(rows, 38.5, 61.2), [.. recs]);
    }
}
