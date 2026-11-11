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
    /// <summary>On the CPU recogniser widths are padded up to a multiple of this (least padding).</summary>
    public const int WidthStep = 64;

    private readonly OcrDetSpec _det;
    private readonly Graph _detGraph;
    private readonly List<Recogniser> _recs = [];
    /// <summary>Accelerators (NPU, GPUs): widths rounded up to OcrMath.RecBuckets and every batch padded to Batch, so
    /// the recognisers see a handful of shapes. A dynamic graph on a GPU recompiles per new shape, and the NPU runs
    /// only static shapes.</summary>
    private readonly bool _fixedShapes;

    public DeviceInfo Device { get; }
    public IReadOnlyList<string> Recognisers => _recs.Select(r => r.Spec.Name).ToList();
    /// <summary>Compiled graphs so far (per-shape devices grow this as new shapes come in).</summary>
    public int CompiledGraphs => _detGraph.Count + _recs.Sum(r => r.Graph.Count);

    private sealed record Recogniser(OcrRecSpec Spec, Graph Graph, string[] Table) : IDisposable
    {
        public void Dispose() => Graph.Dispose();
    }

    /// <summary>One ONNX graph on one device. Dynamic devices compile it once. The NPU compiles a static copy per input
    /// shape on first use (OpenVINO reshape), cached on disk in cacheDir, so later worker starts import the blobs.</summary>
    private sealed class Graph : IDisposable
    {
        private readonly IInferenceBackend _backend;
        private readonly DeviceInfo _device;
        private readonly string _path, _input;
        private readonly string? _cacheDir;
        private readonly IInferenceSession? _dynamic;
        private readonly Dictionary<string, IInferenceSession> _byShape = [];
        private const int MaxShapes = 48;

        public Graph(IInferenceBackend backend, DeviceInfo device, string path, string input, string? cacheDir, bool perShape)
        {
            (_backend, _device, _path, _input, _cacheDir) = (backend, device, path, input, cacheDir);
            if (!perShape) _dynamic = backend.Load(path, device, cacheDir);
        }

        public int Count => _dynamic is null ? _byShape.Count : 1;

        public IInferenceSession For(long[] shape)
        {
            if (_dynamic is not null) return _dynamic;
            string key = string.Join("x", shape);
            if (_byShape.TryGetValue(key, out var s)) return s;
            if (_byShape.Count >= MaxShapes) throw new DeviceFailedException($"{_device.Label} ocr: more than {MaxShapes} input shapes");
            s = _backend.Load(_path, _device, _cacheDir, new Dictionary<string, long[]> { [_input] = shape });
            _byShape[key] = s;
            return s;
        }

        public void Dispose()
        {
            _dynamic?.Dispose();
            foreach (var s in _byShape.Values) s.Dispose();
            _byShape.Clear();
        }
    }

    /// <summary>Loads the detector and the given recognisers (the first is main). Throws DeviceFailedException.</summary>
    public PaddleOcr(IInferenceBackend backend, DeviceInfo device, ModelManifest manifest, IReadOnlyList<OcrRecSpec> recs, string? cacheDir,
        bool? fixedShapes = null)
    {
        Device = device;
        var spec = manifest.Ocr ?? throw new InvalidDataException("ocr manifest lacks ocr");
        _det = spec.Det;
        if (recs.Count == 0) throw new InvalidDataException("ocr manifest has no recogniser");
        bool perShape = device.Kind == DeviceKind.Npu;
        _fixedShapes = fixedShapes ?? device.Kind != DeviceKind.Cpu;
        _detGraph = new Graph(backend, device, manifest.PathOf(_det.File), _det.Input, cacheDir, perShape);
        try
        {
            foreach (var r in recs)
            {
                var dict = LoadDict(manifest.PathOf(r.Dict));
                var table = OcrMath.Table(dict);
                if (r.Classes > 0 && table.Length != r.Classes)
                    throw new InvalidDataException($"ocr {r.Name}: dictionary gives {table.Length} classes, manifest {r.Classes}");
                _recs.Add(new Recogniser(r, new Graph(backend, device, manifest.PathOf(r.File), r.Input, cacheDir, perShape), table));
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
        var outp = _detGraph.For([1, 3, dh, dw]).Run([HostTensor.F(_det.Input, OcrMath.BgrTensor(sized, _det.Mean, _det.Std), 1, 3, dh, dw)]);
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
            int width = _fixedShapes ? OcrMath.RecBucket(widths.Max()) : Math.Min(3200, (widths.Max() + WidthStep - 1) / WidthStep * WidthStep);
            int n = _fixedShapes ? Batch : idx.Count;
            var resized = idx.Select((i, k) => ImagePipeline.Resize(crops[i], Math.Min(widths[k], width), height, "bilinear")).ToList();
            var tensor = OcrMath.RecTensor(resized, height, width, n);
            var outp = r.Graph.For([n, 3, height, width]).Run([HostTensor.F(r.Spec.Input, tensor, n, 3, height, width)]);
            var t = outp.Values.FirstOrDefault() ?? throw new DeviceFailedException("ocr recogniser output missing");
            if (t.Floats is null || t.Shape.Length != 3 || t.Shape[0] != n) throw new DeviceFailedException("ocr recogniser output shape");
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
        _detGraph?.Dispose();
        foreach (var r in _recs) r.Dispose();
        _recs.Clear();
    }
}
