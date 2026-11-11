// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using BetterRecall.Contracts;
using BetterRecall.Worker.Inference;
using BetterRecall.Worker.Protocol;

namespace BetterRecall.Worker.Models.Ocr;

/// <summary>PaddleOCR on one device: the DB detector finds line boxes, the "main" recogniser (PP-OCRv6: Chinese,
/// English, Japanese, Latin scripts) reads every line, and lines it reads with low confidence go through the other
/// recognisers ocr.languages picked (Korean, Cyrillic, Arabic, ...); the most confident reading wins (main gets a
/// small bonus, since confidences of different dictionaries are not calibrated against each other).</summary>
public sealed class PaddleOcr : IDisposable
{
    public const float RetryBelow = 0.85f;
    public const float MainBonus = 0.05f;
    public const int Batch = 8;
    /// <summary>Recogniser widths are padded up to a multiple of this, so a GPU sees a few shapes only.</summary>
    public const int WidthStep = 64;

    private readonly OcrDetSpec _det;
    private readonly IInferenceSession _detSession;
    private readonly List<Recogniser> _recs = [];

    public DeviceInfo Device { get; }
    public IReadOnlyList<string> Recognisers => _recs.Select(r => r.Spec.Name).ToList();

    private sealed record Recogniser(OcrRecSpec Spec, IInferenceSession Session, string[] Table) : IDisposable
    {
        public void Dispose() => Session.Dispose();
    }

    /// <summary>Loads the detector and the given recognisers (the first is main). Throws DeviceFailedException.</summary>
    public PaddleOcr(IInferenceBackend backend, DeviceInfo device, ModelManifest manifest, IReadOnlyList<OcrRecSpec> recs, string? cacheDir)
    {
        Device = device;
        var spec = manifest.Ocr ?? throw new InvalidDataException("ocr manifest lacks ocr");
        _det = spec.Det;
        if (recs.Count == 0) throw new InvalidDataException("ocr manifest has no recogniser");
        _detSession = backend.Load(manifest.PathOf(_det.File), device, cacheDir);
        try
        {
            foreach (var r in recs)
            {
                var dict = LoadDict(manifest.PathOf(r.Dict));
                var table = OcrMath.Table(dict);
                if (r.Classes > 0 && table.Length != r.Classes)
                    throw new InvalidDataException($"ocr {r.Name}: dictionary gives {table.Length} classes, manifest {r.Classes}");
                _recs.Add(new Recogniser(r, backend.Load(manifest.PathOf(r.File), device, cacheDir), table));
            }
        }
        catch
        {
            Dispose();
            throw;
        }
    }

    /// <summary>Dictionary file: one entry per line, UTF-8; CRLF line ends are accepted.</summary>
    internal static string[] LoadDict(string path)
        => File.ReadAllText(path, System.Text.Encoding.UTF8).Replace("\r\n", "\n").Split('\n');

    /// <summary>One read of a small synthetic frame (dark bars on white), so a device that cannot run the graphs fails
    /// here and not on the first screenshot.</summary>
    public void WarmUp()
    {
        const int w = 320, h = 96;
        var rgb = new byte[w * h * 3];
        Array.Fill(rgb, (byte)255);
        for (int y = 40; y < 56; y++)
            for (int x = 20; x < 300; x++)
                if ((x / 6) % 2 == 0) { int o = (y * w + x) * 3; rgb[o] = rgb[o + 1] = rgb[o + 2] = 20; }
        Read(new RgbImage(w, h, rgb));
    }

    public OcrLine[] Read(RgbImage img)
    {
        var (dw, dh) = OcrMath.DetSize(img.Width, img.Height, _det.MaxSide);
        var sized = ImagePipeline.Resize(img, dw, dh, "bilinear");
        var outp = _detSession.Run([HostTensor.F(_det.Input, OcrMath.BgrTensor(sized, _det.Mean, _det.Std), 1, 3, dh, dw)]);
        var prob = outp.Values.FirstOrDefault()?.Floats ?? throw new DeviceFailedException("ocr detector output missing");
        if (prob.Length < dw * dh) throw new DeviceFailedException("ocr detector output too small");
        foreach (var v in prob.AsSpan(0, Math.Min(prob.Length, 4096)))
            if (!float.IsFinite(v)) throw new DeviceFailedException($"{Device.Label} ocr detector output not finite");
        var boxes = OcrMath.Boxes(prob, dw, dh, img.Width, img.Height, _det.Thresh, _det.BoxThresh, _det.Unclip);
        if (boxes.Count == 0) return [];

        var crops = boxes.Select(b => ImagePipeline.Crop(img, b.X, b.Y, b.W, b.H)).ToList();
        var best = Recognise(_recs[0], crops, boxes);
        if (_recs.Count > 1)
        {
            var retry = Enumerable.Range(0, best.Length).Where(i => best[i] is null || best[i]!.Confidence < RetryBelow).ToList();
            var score = best.Select(l => l is null ? -1f : l.Confidence + MainBonus).ToArray();
            if (retry.Count > 0)
                foreach (var r in _recs.Skip(1))
                {
                    var alt = Recognise(r, retry.Select(i => crops[i]).ToList(), retry.Select(i => boxes[i]).ToList());
                    for (int k = 0; k < retry.Count; k++)
                        if (alt[k] is { } a && a.Confidence > score[retry[k]]) { best[retry[k]] = a; score[retry[k]] = a.Confidence; }
                }
        }
        return best.Where(l => l is not null && l.Text.Trim().Length > 0).Select(l => l!).ToArray();
    }

    /// <summary>Reads crops in batches of similar width. Null where nothing was read.</summary>
    private OcrLine?[] Recognise(Recogniser r, IReadOnlyList<RgbImage> crops, IReadOnlyList<TextBox> boxes)
    {
        int height = r.Spec.Height;
        var result = new OcrLine?[crops.Count];
        var order = Enumerable.Range(0, crops.Count).OrderBy(i => (double)crops[i].Width / crops[i].Height).ToList();
        for (int s = 0; s < order.Count; s += Batch)
        {
            var idx = order.Skip(s).Take(Batch).ToList();
            var widths = idx.Select(i => OcrMath.RecWidth(crops[i].Width, crops[i].Height, height)).ToArray();
            int width = Math.Min(3200, (widths.Max() + WidthStep - 1) / WidthStep * WidthStep);
            var resized = idx.Select((i, k) => ImagePipeline.Resize(crops[i], Math.Min(widths[k], width), height, "bilinear")).ToList();
            var outp = r.Session.Run([HostTensor.F(r.Spec.Input, OcrMath.RecTensor(resized, height, width), idx.Count, 3, height, width)]);
            var t = outp.Values.FirstOrDefault() ?? throw new DeviceFailedException("ocr recogniser output missing");
            if (t.Floats is null || t.Shape.Length != 3 || t.Shape[0] != idx.Count) throw new DeviceFailedException("ocr recogniser output shape");
            int steps = checked((int)t.Shape[1]), classes = checked((int)t.Shape[2]);
            if (classes != r.Table.Length) throw new DeviceFailedException($"ocr {r.Spec.Name}: {classes} classes, dictionary {r.Table.Length}");
            for (int k = 0; k < idx.Count; k++)
            {
                var probs = t.Floats.AsSpan(k * steps * classes, steps * classes);
                var ctc = OcrMath.Ctc(probs, steps, classes, r.Table);
                if (ctc.Text.Length == 0) continue;
                if (!float.IsFinite(ctc.Confidence)) throw new DeviceFailedException($"{Device.Label} ocr recogniser output not finite");
                var b = boxes[idx[k]];
                var cuts = OcrMath.Cuts(ctc, steps, width, resized[k].Width, b.X, b.W);
                result[idx[k]] = new OcrLine(ctc.Text, b.X, b.Y, b.W, b.H, ctc.Confidence, cuts);
            }
        }
        return result;
    }

    public void Dispose()
    {
        _detSession?.Dispose();
        foreach (var r in _recs) r.Dispose();
        _recs.Clear();
    }
}
