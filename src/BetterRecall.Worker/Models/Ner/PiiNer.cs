// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using BetterRecall.Contracts;
using BetterRecall.Worker.Inference;
using BetterRecall.Worker.Protocol;

namespace BetterRecall.Worker.Models.Ner;

/// <summary>Person names and postal addresses in OCR lines: the manifest's token classifier (BIO tags) over the lines
/// joined with newlines, in windows of at most MaxTokens tokens cut at line ends. Recall first: a token takes the most
/// likely person/address tag when that tag's probability reaches MinTagProbability, even when "O" is more likely.</summary>
public sealed class PiiNer : IDisposable
{
    public const float MinTagProbability = 0.3f;

    private readonly NerSpec _spec;
    private readonly IInferenceSession _session;
    private readonly BpeTokenizer _tok;
    /// <summary>Per label index: entity (person | address) and whether it begins a span (B-), or null.</summary>
    private readonly (string Entity, string Base, bool Begin)?[] _tags;

    public DeviceInfo Device => _session.Device;

    public PiiNer(IInferenceBackend backend, DeviceInfo device, ModelManifest manifest, string? cacheDir = null)
    {
        _spec = manifest.Ner ?? throw new InvalidDataException("ner manifest lacks ner");
        if (_spec.Labels.Length == 0) throw new InvalidDataException("ner manifest has no labels");
        _tags = Tags(_spec);
        _tok = BpeTokenizer.Load(manifest.PathOf(_spec.Tokenizer));
        _session = backend.Load(manifest.PathOf(_spec.File), device, cacheDir);
    }

    internal static (string Entity, string Base, bool Begin)?[] Tags(NerSpec spec)
        => spec.Labels.Select(l =>
        {
            bool prefixed = l.Length > 2 && l[1] == '-' && l[0] is 'B' or 'I' or 'E' or 'S';
            string b = prefixed ? l[2..] : l;
            return spec.Entities.TryGetValue(b, out var e) ? ((string, string, bool)?)(e, b, !prefixed || l[0] is 'B' or 'S') : null;
        }).ToArray();

    /// <summary>Entities per line (same order as lines); lines without entities get null.</summary>
    public OcrEntity[]?[] Find(IReadOnlyList<string> lines)
    {
        var result = new OcrEntity[]?[lines.Count];
        if (lines.Count == 0) return result;
        var lineStart = new int[lines.Count];
        var sb = new System.Text.StringBuilder();
        for (int i = 0; i < lines.Count; i++)
        {
            if (i > 0) sb.Append('\n');
            lineStart[i] = sb.Length;
            sb.Append(lines[i].Replace('\n', ' '));
        }
        string doc = sb.ToString();
        var tokens = _tok.Encode(doc);
        var spans = new List<(int Start, int End, string Entity, float Score)>();
        int max = Math.Max(16, _spec.MaxTokens - 2);
        for (int from = 0; from < tokens.Count;)
        {
            int to = Math.Min(tokens.Count, from + max);
            if (to < tokens.Count)
            {
                // Cut after the last line break in the window, when there is one past its middle.
                for (int k = to - 1; k > from + max / 2; k--)
                    if (doc.AsSpan(tokens[k].Start, tokens[k].End - tokens[k].Start).Contains('\n')) { to = k + 1; break; }
            }
            spans.AddRange(Run(tokens, from, to));
            from = to;
        }
        foreach (var (s, e, entity, score) in spans)
            for (int i = 0; i < lines.Count; i++)
            {
                int ls = lineStart[i], le = ls + lines[i].Length;
                int a = Math.Max(s, ls), b = Math.Min(e, le);
                if (b <= a) continue;
                var list = result[i] is null ? new List<OcrEntity>() : [.. result[i]!];
                list.Add(new OcrEntity(a - ls, b - a, entity, score));
                result[i] = list.ToArray();
            }
        return result;
    }

    private List<(int, int, string, float)> Run(List<BpeToken> tokens, int from, int to)
    {
        int n = to - from + 2;
        var ids = new long[n];
        var mask = new long[n];
        ids[0] = _tok.BosId; ids[^1] = _tok.EosId;
        for (int i = 0; i < n; i++) mask[i] = 1;
        for (int i = from; i < to; i++) ids[i - from + 1] = tokens[i].Id;
        var feed = new List<HostTensor> { HostTensor.L(_spec.InputIds, ids, 1, n) };
        if (_session.Inputs.Any(x => x.Name == _spec.AttentionMask)) feed.Add(HostTensor.L(_spec.AttentionMask, mask, 1, n));
        var outp = _session.Run(feed);
        if (!outp.TryGetValue(_spec.Output, out var t) || t.Floats is null || t.Shape.Length != 3 || t.Shape[1] != n)
            throw new DeviceFailedException("ner output missing");
        int labels = checked((int)t.Shape[2]);
        if (labels != _spec.Labels.Length) throw new DeviceFailedException($"ner output has {labels} labels, manifest {_spec.Labels.Length}");
        var tags = new (int Tag, float P)[to - from];
        for (int i = 0; i < tags.Length; i++)
            tags[i] = Pick(t.Floats.AsSpan((i + 1) * labels, labels), _tags);
        return Spans(tokens, from, tags, _tags);
    }

    /// <summary>Softmax over one token's logits; the most likely mapped tag when its probability reaches
    /// MinTagProbability, else (-1, 0).</summary>
    internal static (int Tag, float P) Pick(ReadOnlySpan<float> logits, (string Entity, string Base, bool Begin)?[] map)
    {
        float mx = float.NegativeInfinity;
        foreach (var v in logits) if (v > mx) mx = v;
        if (!float.IsFinite(mx)) throw new DeviceFailedException("ner output not finite");
        double sum = 0;
        foreach (var v in logits) sum += Math.Exp(v - mx);
        int best = -1; double bp = 0;
        for (int k = 0; k < logits.Length; k++)
        {
            if (map[k] is null) continue;
            double p = Math.Exp(logits[k] - mx) / sum;
            if (p > bp) { bp = p; best = k; }
        }
        return bp >= MinTagProbability ? (best, (float)bp) : (-1, 0f);
    }

    /// <summary>Consecutive tagged tokens of the same base label form one span (a B- tag starts a new one). A span may
    /// start with the space its first token carries; the agent trims spans before redacting.</summary>
    internal static List<(int, int, string, float)> Spans(IReadOnlyList<BpeToken> tokens, int from, (int Tag, float P)[] tags,
        (string Entity, string Base, bool Begin)?[] map)
    {
        var spans = new List<(int, int, string, float)>();
        int s = -1, e = -1; string? ent = null, bas = null; double score = 0; int count = 0;
        void Flush()
        {
            if (ent is not null && e > s) spans.Add((s, e, ent, (float)(score / count)));
            ent = null; bas = null; score = 0; count = 0;
        }
        for (int i = 0; i < tags.Length; i++)
        {
            var tok = tokens[from + i];
            var (tag, p) = tags[i];
            if (tag < 0 || map[tag] is not { } m) { Flush(); continue; }
            if (ent is null || m.Begin || m.Base != bas) { Flush(); s = tok.Start; e = tok.Start; ent = m.Entity; bas = m.Base; }
            e = Math.Max(e, tok.End);
            score += p; count++;
        }
        Flush();
        return spans;
    }

    public void Dispose() => _session.Dispose();
}
