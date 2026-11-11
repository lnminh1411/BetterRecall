// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using BetterRecall.Worker.Inference;
using BetterRecall.Worker.Protocol;

namespace BetterRecall.Worker.Models;

/// <summary>Small image classifier run before embedding (INT8 graph on the CPU, the FP32 AcceleratorFile on an NPU or
/// GPU; the device comes from <see cref="Inference.NsfwDevicePlan"/>). Score = summed softmax of the
/// manifest's BlockLabels. A whole screen squeezed to the model input (224 px) loses small windows and thumbnails,
/// so <see cref="Score"/> can also score tiles (grid 2: quadrants plus a centre tile over their seams; grid 3: a 3x3
/// grid) and keeps the highest. On a test set placed at quarter screen width, grid 1/2/3 blocked 16/61/71 of 81.</summary>
public sealed class NsfwClassifier : IDisposable
{
    private readonly IInferenceSession _session;
    private readonly ClassifierSpec _spec;
    private readonly int[] _block;

    public ModelManifest Manifest { get; }

    public DeviceInfo Device => _session.Device;

    /// <summary>Loads the graph for <paramref name="device"/>: File on the CPU, AcceleratorFile (or File) elsewhere.
    /// The input is fixed to [1,3,H,W] so the NPU gets a static shape. Throws DeviceFailedException.</summary>
    public NsfwClassifier(IInferenceBackend backend, DeviceInfo device, ModelManifest manifest, string? cacheDir = null)
    {
        Manifest = manifest;
        _spec = manifest.Classifier ?? throw new InvalidDataException("nsfw manifest lacks classifier");
        _block = _spec.BlockLabels.Select(l => Array.IndexOf(_spec.Labels, l)).Where(i => i >= 0).ToArray();
        if (_block.Length == 0) throw new InvalidDataException("nsfw manifest names no block label");
        _session = backend.Load(manifest.PathOf(FileFor(_spec, device.Kind)), device, cacheDir,
            new Dictionary<string, long[]> { [_spec.Input] = [1, 3, _spec.Height, _spec.Width] });
    }

    /// <summary>The INT8 graph is for the CPU; accelerators take the FP32 graph when the manifest has one.</summary>
    internal static string FileFor(ClassifierSpec spec, DeviceKind kind)
        => kind == DeviceKind.Cpu || string.IsNullOrEmpty(spec.AcceleratorFile) ? spec.File : spec.AcceleratorFile;

    /// <summary>One run on a zero frame: compiles lazily-built kernels and proves the device produces a finite score.</summary>
    public void WarmUp()
    {
        var p = Run(new float[3 * _spec.Height * _spec.Width]);
        if (!double.IsFinite(p)) throw new DeviceFailedException($"{Device.Label} nsfw classifier output not finite");
    }

    /// <summary>Highest block score over the full frame and its tiles (see <see cref="Tiles"/>). Stops early once a
    /// score reaches <paramref name="stopAt"/>, so a frame the full view already blocks costs one run.</summary>
    public double Score(RgbImage image, double stopAt = double.PositiveInfinity, int grid = 1)
    {
        double best = ScoreOne(image);
        foreach (var (x, y, w, h) in Tiles(image.Width, image.Height, _spec.Width, _spec.Height, grid))
        {
            if (best >= stopAt) break;
            best = Math.Max(best, ScoreOne(ImagePipeline.Crop(image, x, y, w, h)));
        }
        return best;
    }

    /// <summary>Tiles scored after the full frame. Grid 2: four quadrants, then a centre tile of the same size over
    /// their seams. Grid 3: a 3x3 grid, or grid 2 when a ninth would be smaller than the model input. None when even a
    /// quadrant would be smaller, since the full frame already sees everything at native size or larger.</summary>
    internal static IEnumerable<(int X, int Y, int W, int H)> Tiles(int width, int height, int inW, int inH, int grid)
    {
        if (grid >= 3 && width / 3 >= inW && height / 3 >= inH)
        {
            int w3 = width / 3, h3 = height / 3;
            for (int r = 0; r < 3; r++)
                for (int c = 0; c < 3; c++)
                    yield return (c * w3, r * h3, w3, h3);
            yield break;
        }
        if (grid < 2) yield break;
        int w = width / 2, h = height / 2;
        if (w < inW || h < inH) yield break;
        yield return (0, 0, w, h);
        yield return (w, 0, w, h);
        yield return (0, h, w, h);
        yield return (w, h, w, h);
        yield return (w / 2, h / 2, w, h);
    }

    private double ScoreOne(RgbImage image)
    {
        double p = Run(ImagePipeline.ToTensor(image, _spec.Height, _spec.Width, _spec.Preprocess));
        // A NaN would compare false against the threshold and let the frame through: fail the request instead.
        if (!double.IsFinite(p)) throw new DeviceFailedException($"{Device.Label} nsfw classifier output not finite");
        return p;
    }

    private double Run(float[] x)
    {
        var outp = _session.Run([HostTensor.F(_spec.Input, x, 1, 3, _spec.Height, _spec.Width)]);
        if (!outp.TryGetValue(_spec.Output, out var logits) || logits.Floats is null || logits.Floats.Length < _spec.Labels.Length)
            throw new DeviceFailedException("nsfw classifier output missing");
        var z = logits.Floats.AsSpan(0, _spec.Labels.Length);
        foreach (var v in z) if (!float.IsFinite(v)) return double.NaN;
        return BlockProbability(z, _block);
    }

    internal static double BlockProbability(ReadOnlySpan<float> logits, int[] block)
    {
        double max = double.NegativeInfinity;
        foreach (var l in logits) max = Math.Max(max, l);
        double sum = 0, hit = 0;
        for (int i = 0; i < logits.Length; i++)
        {
            double e = Math.Exp(logits[i] - max);
            sum += e;
            if (Array.IndexOf(block, i) >= 0) hit += e;
        }
        return sum > 0 ? hit / sum : 0;
    }

    public void Dispose() => _session.Dispose();
}
