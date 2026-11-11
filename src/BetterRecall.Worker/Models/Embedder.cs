// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using BetterRecall.Worker.Engine;
using BetterRecall.Worker.Inference;
using BetterRecall.Worker.Protocol;
using BetterRecall.Worker.Vectors;

namespace BetterRecall.Worker.Models;

/// <summary>Manifest-driven image/text embedder. Vision graphs run on Device; text graphs (and the image's second pass)
/// on TextDevice, which equals Device unless the benchmark chose a split (vision on the NPU, text on the CPU).
/// EmbeddingGemma 2: the vision tower makes soft tokens, then the text transformer runs over them (two passes); the
/// token embedding table lives outside the graphs and one dynamic text graph is compiled per static length.
/// SigLIP 2: separate image and text towers.</summary>
public sealed class Embedder : IDisposable
{
    private readonly IInferenceBackend _backend;
    private readonly string? _cacheDir;
    private readonly GemmaTokenizer _tokenizer;
    private readonly EmbeddingTable? _table;
    private readonly List<TextGraph> _text;
    private readonly VisionGraph _vision;
    private readonly Dictionary<string, IInferenceSession> _sessions = new(StringComparer.OrdinalIgnoreCase);

    public ModelManifest Manifest { get; }
    public DeviceInfo Device { get; }
    public DeviceInfo TextDevice { get; }
    public int ImageTokens => _vision.Tokens;
    /// <summary>Text lengths dropped because TextDevice failed to compile them (NPU compiler limits).</summary>
    public IReadOnlyList<int> DroppedLengths => _dropped;
    private readonly List<int> _dropped = [];

    public Embedder(IInferenceBackend backend, DeviceInfo device, ModelManifest manifest, int imageTokens, string? cacheDir, DeviceInfo? textDevice = null)
    {
        _backend = backend; Device = device; Manifest = manifest; _cacheDir = cacheDir; TextDevice = textDevice ?? device;
        if (manifest.Tokenizer is null || manifest.Text.Count == 0 || manifest.Vision.Count == 0)
            throw new InvalidDataException($"manifest {manifest.Name} lacks tokenizer, text or vision graphs");
        _tokenizer = new GemmaTokenizer(manifest.PathOf(manifest.Tokenizer.File), manifest.Tokenizer);
        _text = manifest.Text.OrderBy(t => t.Seq).ToList();
        _vision = PickVision(manifest.Vision, imageTokens);
        bool needTable = _text.Any(t => t.InputsEmbeds is not null) || _vision.Fusion?.InputsEmbeds is not null;
        if (needTable)
        {
            var spec = manifest.Embeddings ?? throw new InvalidDataException("manifest needs an embedding table");
            _table = new EmbeddingTable(manifest.PathOf(spec.File), spec);
        }
    }

    internal static VisionGraph PickVision(List<VisionGraph> graphs, int tokens)
        => graphs.FirstOrDefault(v => v.Tokens == tokens) ?? graphs.OrderBy(v => Math.Abs(v.Tokens - tokens)).First();

    private IInferenceSession Session(string file, IReadOnlyDictionary<string, long[]>? shapes, DeviceInfo device)
    {
        file = Manifest.FileFor(file, _backend is Inference.Onnx.OnnxBackend);
        string key = device.Id + "|" + file + (shapes is null ? "" : "|" + string.Join(";", shapes.Select(kv => kv.Key + "=" + string.Join("x", kv.Value))));
        if (_sessions.TryGetValue(key, out var s)) return s;
        s = _backend.Load(Manifest.PathOf(file), device, _cacheDir, shapes);
        _sessions[key] = s;
        return s;
    }

    private static Dictionary<string, long[]>? EmbedShapes(string? embedsName, string? maskName, int seq, int dim)
    {
        if (embedsName is null) return null;
        var d = new Dictionary<string, long[]> { [embedsName] = [1, seq, dim] };
        if (maskName is not null) d[maskName] = [1, seq];
        return d;
    }

    private Dictionary<string, long[]>? TextShapes(TextGraph g) => EmbedShapes(g.InputsEmbeds, g.AttentionMask, g.Seq, _table?.Dim ?? 0);

    /// <summary>Soft tokens of a fixed-size vision graph; for the patch form, the most it can emit (the budget).</summary>
    private static int MaxSoftTokens(VisionGraph v) => v.PositionsInput is not null ? v.Tokens : v.SoftTokens > 0 ? v.SoftTokens : v.Tokens;

    /// <summary>Real length of the image sequence for n soft tokens.</summary>
    private static int FusionLength(FusionGraph f, int n) => f.PrefixIds.Length + n + f.SuffixIds.Length;

    /// <summary>Compiled length for the image sequence: the smallest text length of the same graph that fits (padded and
    /// masked), so the image pass reuses a text compile; the exact length when none fits.</summary>
    private int FusionSeq(FusionGraph f, int n)
    {
        int len = FusionLength(f, n);
        return _text.Where(t => t.InputsEmbeds is not null && string.Equals(t.File, f.File, StringComparison.OrdinalIgnoreCase) && t.Seq >= len)
                    .Select(t => t.Seq).DefaultIfEmpty(len).Min();
    }

    /// <summary>Compiles every graph this embedder will use (text lengths, the vision graph and its second pass). On an
    /// accelerator, a text length that fails to compile is dropped (texts that would use it go to the next longer one),
    /// as long as one length stays; the NPU 3720 compiler rejects short sequences of the EmbeddingGemma 2 text graph.</summary>
    public void LoadAll(bool text = true, bool vision = true)
    {
        if (vision) Session(_vision.File, null, Device);
        if (text)
            foreach (var t in _text.ToList())
            {
                try { Session(t.File, TextShapes(t), TextDevice); }
                catch (DeviceFailedException) when (TextDevice.Kind != DeviceKind.Cpu && t.InputsEmbeds is not null && _text.Count > 1 && !ReferenceEquals(t, _text[^1]))
                {
                    _text.Remove(t);
                    _dropped.Add(t.Seq);
                    Hosting.Log.Info($"{Manifest.Name}: text length {t.Seq} does not compile on {TextDevice.Id}; using longer lengths");
                }
            }
        if (vision && _vision.Fusion is { } f)
            Session(f.File, EmbedShapes(f.InputsEmbeds, f.AttentionMaskInput, FusionSeq(f, MaxSoftTokens(_vision)), _table?.Dim ?? 0), TextDevice);
    }

    /// <summary>Token budgets this model offers (vision graphs), ascending.</summary>
    public IReadOnlyList<int> Budgets => Manifest.Vision.Select(v => v.Tokens).Distinct().Order().ToList();

    public float[] EmbedText(string text, TextKind kind)
    {
        var prompts = Manifest.Prompts ?? new PromptSpec();
        string prompt = (kind == TextKind.Query ? prompts.Query : prompts.Document).Replace("{text}", text, StringComparison.Ordinal);
        var ids = _tokenizer.Encode(prompt, _text[^1].Seq);
        var graph = Manifest.Tokenizer!.PadToMax ? _text[^1] : _text.First(t => t.Seq >= ids.Length);
        var padded = new long[graph.Seq];
        var mask = new long[graph.Seq];
        Array.Fill(padded, _tokenizer.PadId);
        for (int i = 0; i < ids.Length; i++) { padded[i] = ids[i]; mask[i] = 1; }

        var feed = new List<HostTensor>();
        if (graph.InputsEmbeds is not null)
        {
            var emb = new float[graph.Seq * _table!.Dim];
            _table.Gather(padded, emb);
            feed.Add(HostTensor.F(graph.InputsEmbeds, emb, 1, graph.Seq, _table.Dim));
        }
        else feed.Add(HostTensor.L(graph.InputIds, padded, 1, graph.Seq));
        if (graph.AttentionMask is not null) feed.Add(HostTensor.L(graph.AttentionMask, mask, 1, graph.Seq));
        return Take(Session(graph.File, TextShapes(graph), TextDevice).Run(feed), graph.Output);
    }

    /// <summary>Embeds an image at the configured token budget, or at <paramref name="tokens"/> (bench) when given.</summary>
    public float[] EmbedImage(RgbImage image, int? tokens = null)
    {
        var v = tokens is { } t ? PickVision(Manifest.Vision, t) : _vision;
        IReadOnlyDictionary<string, HostTensor> vout;
        int n;
        if (v.PositionsInput is not null)
        {
            var (patches, pos, soft0, _, _, pool) = ImagePipeline.ToPatches(image, v);
            int m = v.PatchBudget;
            var inputs = new List<HostTensor>
            {
                HostTensor.F(v.Input, patches, 1, m, patches.Length / m),
                HostTensor.L(v.PositionsInput, pos, 1, m, 2),
            };
            if (v.PoolInput is not null)
                inputs.Add(HostTensor.F(v.PoolInput, pool!, 1, v.Tokens, m));
            vout = Session(v.File, null, Device).Run(inputs);
            n = soft0;
        }
        else
        {
            var pixels = ImagePipeline.ToTensor(image, v.Height, v.Width, v.Preprocess);
            vout = Session(v.File, null, Device).Run([HostTensor.F(v.Input, pixels, 1, 3, v.Height, v.Width)]);
            n = MaxSoftTokens(v);
        }
        if (v.Fusion is null) return Take(vout, v.Output);

        if (!vout.TryGetValue(v.Output, out var soft) || soft.Floats is null) throw new DeviceFailedException($"vision output {v.Output} missing");
        var f = v.Fusion;
        if (f.InputsEmbeds is not null)
        {
            int dim = _table!.Dim, slots = (int)(soft.Floats.Length / dim);
            if (slots != MaxSoftTokens(v)) throw new DeviceFailedException($"vision tower gave {slots} soft tokens, manifest says {MaxSoftTokens(v)}");
            if (n > slots) throw new DeviceFailedException($"image needs {n} soft tokens, graph has {slots}");
            if (VectorMath.HasNonFinite(soft.Floats.AsSpan(0, n * dim))) throw new DeviceFailedException($"{Device.Label} vision tower output contains NaN");
            int len = FusionLength(f, n), seq = FusionSeq(f, n);
            var ids = new long[seq];
            Array.Fill(ids, _tokenizer.PadId);
            var emb = new float[seq * dim];
            _table.Gather(f.PrefixIds, emb);
            soft.Floats.AsSpan(0, n * dim).CopyTo(emb.AsSpan(f.PrefixIds.Length * dim));
            _table.Gather(f.SuffixIds, emb.AsSpan((f.PrefixIds.Length + n) * dim));
            if (seq > len) _table.Gather(ids.AsSpan(len), emb.AsSpan(len * dim));
            var mask = new long[seq];
            Array.Fill(mask, 1L, 0, len);
            var feed = new List<HostTensor> { HostTensor.F(f.InputsEmbeds, emb, 1, seq, dim) };
            if (f.AttentionMaskInput is not null) feed.Add(HostTensor.L(f.AttentionMaskInput, mask, 1, seq));
            return Take(Session(f.File, EmbedShapes(f.InputsEmbeds, f.AttentionMaskInput, seq, dim), TextDevice).Run(feed), f.Output);
        }
        else
        {
            if (VectorMath.HasNonFinite(soft.Floats)) throw new DeviceFailedException($"{Device.Label} vision tower output contains NaN");
            var feed = new List<HostTensor> { HostTensor.F(f.ImageEmbedsInput, soft.Floats, soft.Shape) };
            if (f.InputIdsInput is not null) feed.Add(HostTensor.L(f.InputIdsInput, f.InputIds, 1, f.InputIds.Length));
            if (f.AttentionMaskInput is not null)
            {
                var mask = new long[f.InputIds.Length];
                Array.Fill(mask, 1L);
                feed.Add(HostTensor.L(f.AttentionMaskInput, mask, 1, mask.Length));
            }
            return Take(Session(f.File, null, TextDevice).Run(feed), f.Output);
        }
    }

    private float[] Take(IReadOnlyDictionary<string, HostTensor> outputs, string name)
    {
        if (!outputs.TryGetValue(name, out var t) || t.Floats is null) throw new DeviceFailedException($"output {name} missing");
        int dim = Manifest.Dim > 0 ? Manifest.Dim : t.Floats.Length;
        if (t.Floats.Length < dim) throw new DeviceFailedException($"output {name} shorter than {dim}");
        var v = t.Floats.AsSpan(0, dim).ToArray();
        VectorMath.L2NormalizeInPlace(v);
        return v;
    }

    public void Dispose()
    {
        foreach (var s in _sessions.Values) s.Dispose();
        _sessions.Clear();
        _table?.Dispose();
    }
}
