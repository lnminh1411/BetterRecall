// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using BetterRecall.Agent.Util;
using BetterRecall.Contracts;

namespace BetterRecall.Agent.Capture;

/// <summary>
/// Picks the keyframe encoder. keyframes.format avif with keyframes.encoder auto: the iGPU's AV1 hardware encoder, then
/// libaom, then WebP when avif.dll is missing (AVIF of either kind needs it to decode for the UI). hardware: the same
/// order, with a warning when the hardware is not there. software: libaom. A hardware failure on one frame falls back to
/// libaom for that frame; three in a row turn the hardware off until keyframes.encoder or keyframes.quality changes.
/// </summary>
internal sealed unsafe class KeyframeEncoder(Func<GpuDeviceCache?> devices) : IDisposable
{
    private const int MaxFailures = 3;
    private HwAvifEncoder? _hw;
    private bool _hwTried;
    private int _failures;
    private string _mode = "";
    private int _quality = -1;

    /// <summary>Encoder of the last keyframe, e.g. "oneVPL AV1 VDENC on Intel(R) Graphics", "libaom (software)", "WebP".</summary>
    public string Active { get; private set; } = "";

    /// <summary>Why the hardware encoder is not in use, null when it is (or was never wanted).</summary>
    public string? HardwareUnavailable { get; private set; }

    public (byte[] Bytes, string Format) Encode(byte* bgra, int width, int height, int stride, Settings s)
    {
        int quality = Math.Clamp(s.Keyframes.Quality, 30, 95);
        string format = ImageCodec.Resolve(s.Keyframes.Format);
        string mode = s.Keyframes.Encoder is "hardware" or "software" ? s.Keyframes.Encoder : "auto";
        if (mode != _mode || quality != _quality)
        {
            if (_failures >= MaxFailures) { _hwTried = false; _failures = 0; }
            _mode = mode;
            _quality = quality;
        }
        if (format == "avif" && mode != "software" && Hardware() is { } hw)
        {
            var bytes = hw.Encode(bgra, width, height, stride, quality, out var error);
            if (bytes is not null)
            {
                _failures = 0;
                SetActive(hw.Name);
                return (bytes, "avif");
            }
            if (++_failures >= MaxFailures)
            {
                Log.Warn($"keyframe AV1 hardware encoder off after {MaxFailures} failures ({error}); using libaom");
                HardwareUnavailable = error;
                DisposeHw();
            }
            else Log.Debug($"keyframe AV1 hardware encode failed ({error}); libaom for this frame");
        }
        var encoded = ImageCodec.EncodeBgra(bgra, width, height, stride, format, quality);
        SetActive(format switch { "avif" => "libaom (software)", "jxl" => "libjxl", "jpeg" => "JPEG", _ => "WebP" });
        return (encoded, format);
    }

    private HwAvifEncoder? Hardware()
    {
        if (_hw is not null) return _hw;
        if (_hwTried) return null;
        _hwTried = true;
        var d = devices();
        if (d is null) { HardwareUnavailable = "no GPU devices"; return null; }
        _hw = HwAvifEncoder.Create(d, out var why);
        HardwareUnavailable = why;
        if (_hw is null)
        {
            if (_mode == "hardware") Log.Warn($"keyframes.encoder hardware: {why}; using libaom");
            else Log.Info($"keyframe AV1 hardware encoder unavailable ({why}); using libaom");
        }
        return _hw;
    }

    private void SetActive(string name)
    {
        if (name == Active) return;
        Active = name;
        Log.Info($"keyframe encoder: {name}");
    }

    private void DisposeHw()
    {
        _hw?.Dispose();
        _hw = null;
    }

    public void Dispose() => DisposeHw();
}
