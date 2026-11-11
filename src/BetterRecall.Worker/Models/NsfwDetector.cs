// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using BetterRecall.Worker.Inference;
using BetterRecall.Worker.Protocol;

namespace BetterRecall.Worker.Models;

/// <summary>Second NSFW model: the NudeNet YOLOv8 detector (exposed and covered body parts), run at 640 px on the whole
/// frame after the classifier passed it. Catches suggestive frames the classifier scores low. Score = highest score of
/// any block label over all anchors; no NMS, since only the maximum matters.</summary>
public sealed class NsfwDetector : IDisposable
{
    private readonly IInferenceSession _session;
    private readonly DetectorSpec _spec;
    private readonly int[] _blockRows;

    /// <summary>RGB / 255, no mean or std, bilinear (PIL-compatible, as in the offline evaluation).</summary>
    private static readonly ImagePreprocessSpec Pre = new() { Resample = "bilinear", Rescale = 1f / 255f, Mean = [0, 0, 0], Std = [1, 1, 1], Fit = "stretch" };

    public ModelManifest Manifest { get; }
    public DeviceInfo Device => _session.Device;

    /// <summary>Input fixed to [1,3,Size,Size] (the NPU needs a static shape). Throws DeviceFailedException.</summary>
    public NsfwDetector(IInferenceBackend backend, DeviceInfo device, ModelManifest manifest, string? cacheDir = null)
    {
        Manifest = manifest;
        _spec = manifest.Detector ?? throw new InvalidDataException("nsfw-detector manifest lacks detector");
        if (_spec.Size <= 0 || _spec.Labels.Length == 0) throw new InvalidDataException("nsfw-detector manifest has no size or labels");
        _blockRows = BlockRows(_spec);
        if (_blockRows.Length == 0) throw new InvalidDataException("nsfw-detector manifest names no block label");
        _session = backend.Load(manifest.PathOf(_spec.File), device, cacheDir,
            new Dictionary<string, long[]> { [_spec.Input] = [1, 3, _spec.Size, _spec.Size] });
    }

    /// <summary>Output rows of the block labels: 4 box rows come first.</summary>
    internal static int[] BlockRows(DetectorSpec spec)
        => spec.BlockLabels.Select(l => Array.IndexOf(spec.Labels, l)).Where(i => i >= 0).Select(i => 4 + i).Distinct().ToArray();

    public void WarmUp()
    {
        double s = Run(new float[3 * _spec.Size * _spec.Size]);
        if (!double.IsFinite(s)) throw new DeviceFailedException($"{Device.Label} nsfw detector output not finite");
    }

    public double Score(RgbImage image)
    {
        double s = Run(Letterbox(image, _spec.Size));
        if (!double.IsFinite(s)) throw new DeviceFailedException($"{Device.Label} nsfw detector output not finite");
        return s;
    }

    private double Run(float[] x)
    {
        var outp = _session.Run([HostTensor.F(_spec.Input, x, 1, 3, _spec.Size, _spec.Size)]);
        if (!outp.TryGetValue(_spec.Output, out var t) || t.Floats is null || t.Shape.Length != 3)
            throw new DeviceFailedException("nsfw detector output missing");
        int rows = checked((int)t.Shape[1]), anchors = checked((int)t.Shape[2]);
        if (rows != 4 + _spec.Labels.Length) throw new DeviceFailedException($"nsfw detector output has {rows} rows, manifest {4 + _spec.Labels.Length}");
        return BlockScore(t.Floats, rows, anchors, _blockRows);
    }

    /// <summary>Square canvas of side max(w, h), black, image at the top left; bilinear resize to size x size; RGB
    /// /255 in CHW order ([1,3,size,size]).</summary>
    internal static float[] Letterbox(RgbImage image, int size)
    {
        var square = image;
        if (image.Width != image.Height)
        {
            int side = Math.Max(image.Width, image.Height);
            var rgb = new byte[side * side * 3];
            for (int y = 0; y < image.Height; y++)
                Buffer.BlockCopy(image.Rgb, y * image.Width * 3, rgb, y * side * 3, image.Width * 3);
            square = new RgbImage(side, side, rgb);
        }
        return ImagePipeline.ToTensor(square, size, size, Pre);
    }

    /// <summary>Highest value of the block rows over all anchors of a [rows, anchors] output (batch 1). NaN when a
    /// block value is not finite; 0 when no block row exists.</summary>
    internal static double BlockScore(ReadOnlySpan<float> output, int rows, int anchors, int[] blockRows)
    {
        if (output.Length < (long)rows * anchors) throw new DeviceFailedException("nsfw detector output too short");
        double best = 0;
        foreach (int r in blockRows)
        {
            if (r < 0 || r >= rows) continue;
            foreach (var v in output.Slice(r * anchors, anchors))
            {
                if (!float.IsFinite(v)) return double.NaN;
                if (v > best) best = v;
            }
        }
        return best;
    }

    public void Dispose() => _session.Dispose();
}
