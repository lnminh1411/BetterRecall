// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Diagnostics;
using BetterRecall.Agent.Capture;
using BetterRecall.Agent.Native;
using BetterRecall.Agent.Util;
using Vortice.Direct3D11;
using Vortice.MediaFoundation;

namespace BetterRecall.Agent.Replay;

/// <summary>Receives encoded packets in output order. Pts in 100 ns.</summary>
internal delegate void PacketSink(ReadOnlySpan<byte> data, long pts, bool key);

/// <summary>A hardware video encoder fed with NV12 D3D11 textures on its own adapter.</summary>
internal interface IVideoEncoder : IDisposable
{
    VideoCodec Codec { get; }
    /// <summary>"vpl" (oneVPL) or "mf" (Media Foundation hardware MFT).</summary>
    string Api { get; }
    string Name { get; }
    int Width { get; }
    int Height { get; }
    /// <summary>Size of the NV12 texture TrySubmit expects (the picture is its top-left Width x Height).</summary>
    int SurfaceWidth { get; }
    int SurfaceHeight { get; }
    /// <summary>True: TrySubmit copies the texture before returning, so the caller may reuse it at once.</summary>
    bool CopiesInput { get; }
    bool ReadyForInput { get; }
    bool TrySubmit(ID3D11Texture2D nv12, long pts100ns, bool forceKey);
    long FramesIn { get; }
    long PacketsOut { get; }
    long FramesDropped { get; }
    long ForcedKeyframes { get; }
    string? Error { get; }
    LatencyStats Latency { get; }
}

/// <summary>Submit-to-packet times in ms (bounded sample buffer).</summary>
internal sealed class LatencyStats
{
    private readonly object _gate = new();
    private readonly List<double> _ms = new();

    public void Add(double ms)
    {
        lock (_gate)
        {
            if (_ms.Count >= 8192) _ms.RemoveRange(0, 4096);
            _ms.Add(ms);
        }
    }

    public int Count { get { lock (_gate) return _ms.Count; } }
    public void Reset() { lock (_gate) _ms.Clear(); }

    public double Percentile(double p)
    {
        lock (_gate)
        {
            if (_ms.Count == 0) return 0;
            var a = _ms.ToArray();
            Array.Sort(a);
            int i = (int)Math.Ceiling(p / 100.0 * a.Length) - 1;
            return a[Math.Clamp(i, 0, a.Length - 1)];
        }
    }
}

/// <summary>One encoder attempt: adapter, codec and API.</summary>
internal sealed record EncoderCandidate(AdapterInfo Adapter, VideoCodec Codec, string Api);

/// <summary>Encoder selection (pure): codec order, adapter order, candidate list.</summary>
internal static class EncoderPlan
{
    /// <summary>replay.encoder: auto (av1, hevc, h264) | av1 | hevc | h264. The old *_qsv names map to their codec.</summary>
    public static IReadOnlyList<VideoCodec> Codecs(string? preference) => (preference ?? "auto").ToLowerInvariant() switch
    {
        "av1" or "av1_qsv" => [VideoCodec.Av1],
        "hevc" or "hevc_qsv" => [VideoCodec.Hevc],
        "h264" or "h264_qsv" => [VideoCodec.H264],
        _ => [VideoCodec.Av1, VideoCodec.Hevc, VideoCodec.H264],
    };

    /// <summary>APIs to try for a codec on an adapter: AV1 on Intel goes through oneVPL first (the driver has no AV1 MFT).</summary>
    public static IReadOnlyList<string> Apis(AdapterInfo a, VideoCodec c) =>
        c == VideoCodec.Av1 && a.VendorId == GpuDevices.VendorIntel ? ["vpl", "mf"] : ["mf"];

    /// <summary>
    /// replay.encoderAdapter: auto = integrated GPU first, then the GPU driving the most captured monitors, then the rest;
    /// any other value is a case-insensitive substring of the adapter name, tried first, auto order after it.
    /// </summary>
    public static List<AdapterInfo> AdapterOrder(IReadOnlyList<AdapterInfo> adapters, IReadOnlyList<OutputInfo> captured, string? setting)
    {
        var hw = adapters.Where(a => !a.Software).ToList();
        int Driven(AdapterInfo a) => captured.Count(o => o.AdapterLuid == a.Luid);
        var auto = hw.OrderByDescending(a => a.Integrated).ThenByDescending(Driven).ThenBy(a => hw.IndexOf(a)).ToList();
        if (string.IsNullOrWhiteSpace(setting) || setting.Equals("auto", StringComparison.OrdinalIgnoreCase)) return auto;
        var named = auto.Where(a => a.Name.Contains(setting.Trim(), StringComparison.OrdinalIgnoreCase)).ToList();
        return named.Concat(auto.Where(a => !named.Contains(a))).ToList();
    }

    /// <summary>Candidates in try order: adapter-major (all codecs on the preferred adapter first), codec order, API order.</summary>
    public static List<EncoderCandidate> Candidates(IReadOnlyList<AdapterInfo> adapters, IReadOnlyList<OutputInfo> captured,
        string? adapterSetting, string? encoderSetting, Func<AdapterInfo, VideoCodec, string, bool> available)
    {
        var list = new List<EncoderCandidate>();
        foreach (var a in AdapterOrder(adapters, captured, adapterSetting))
            foreach (var c in Codecs(encoderSetting))
                foreach (var api in Apis(a, c))
                    if (available(a, c, api)) list.Add(new EncoderCandidate(a, c, api));
        return list;
    }
}

/// <summary>Creates encoders and probes which codecs each adapter exposes (cached per adapter LUID).</summary>
internal static class EncoderFactory
{
    private static readonly Dictionary<(long, VideoCodec, string), bool> ProbeCache = new();

    public static bool Available(AdapterInfo a, VideoCodec c, string api)
    {
        lock (ProbeCache)
        {
            if (ProbeCache.TryGetValue((a.Luid, c, api), out var ok)) return ok;
            try { ok = api == "vpl" ? VplAv1Encoder.Probe(a.Luid) : MfVideoEncoder.Find(a.Luid, a.VendorId, c) is not null; }
            catch (Exception ex) { Log.Debug($"encoder probe {c}/{api}: {ex.Message}"); ok = false; }
            ProbeCache[(a.Luid, c, api)] = ok;
            return ok;
        }
    }

    /// <summary>"av1:vpl", "hevc:mf", ... for HardwareInfo.</summary>
    public static string[] Codecs(AdapterInfo a)
    {
        var list = new List<string>();
        foreach (var c in new[] { VideoCodec.Av1, VideoCodec.Hevc, VideoCodec.H264 })
            foreach (var api in EncoderPlan.Apis(a, c))
                if (Available(a, c, api)) list.Add($"{CodecName(c)}:{api}");
        return list.ToArray();
    }

    public static string CodecName(VideoCodec c) => c switch { VideoCodec.Av1 => "av1", VideoCodec.Hevc => "hevc", _ => "h264" };

    /// <summary>Largest picture each attempt allows: native first (capped at 16384), then the 7680x4320 assumption, then 4096x2160.</summary>
    public static readonly (int W, int H)[] SizeLimits = [(16384, 16384), (7680, 4320), (4096, 2160)];

    public static IVideoEncoder? Create(EncoderCandidate c, GpuDevice dev, int w, int h, int fps, int kbps, PacketSink sink, out string? error)
    {
        error = null;
        try
        {
            IVideoEncoder? enc = c.Api == "vpl"
                ? VplAv1Encoder.Create(dev, w, h, fps, kbps, sink, out error)
                : MfVideoEncoder.Create(dev, c.Codec, w, h, fps, kbps, sink, out error);
            return enc;
        }
        catch (Exception ex)
        {
            error = ex.Message;
            return null;
        }
    }
}

/// <summary>Bitstream helpers shared by every encoder and the muxer.</summary>
internal static class CodecBits
{
    public static bool DetectKey(VideoCodec codec, ReadOnlySpan<byte> data)
    {
        try
        {
            switch (codec)
            {
                case VideoCodec.Av1:
                    foreach (var o in Av1.Split(data)) if (o.Type == Av1.ObuSequenceHeader) return true;
                    return false;
                case VideoCodec.H264:
                    foreach (var n in AnnexB.Split(data)) if (n.Length > 0 && AnnexB.H264Type(data.Slice(n.Start, n.Length)) == 5) return true;
                    return false;
                default:
                    foreach (var n in AnnexB.Split(data))
                        if (n.Length > 1 && AnnexB.HevcType(data.Slice(n.Start, n.Length)) is 19 or 20 or 21) return true;
                    return false;
            }
        }
        catch { return false; }
    }

    /// <summary>av1C / hvcC / avcC payload from a keyframe, or null when it carries no parameter sets.</summary>
    public static byte[]? BuildConfig(VideoCodec codec, ReadOnlySpan<byte> keyframe)
    {
        try
        {
            return codec switch
            {
                VideoCodec.Av1 => Av1.FindSequenceHeader(keyframe) is { } seq ? Av1.BuildAv1C(seq) : null,
                VideoCodec.Hevc => AnnexB.BuildHvcC(keyframe),
                _ => AnnexB.BuildAvcC(keyframe),
            };
        }
        catch (Exception ex)
        {
            Log.Warn("codec config parse", ex);
            return null;
        }
    }

    public static double TicksToMs(long ticks) => ticks * 1000.0 / Stopwatch.Frequency;
}
