// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Text;
using System.Text.Json;

namespace BetterRecall.Worker.Models.Ner;

/// <summary>One token with its UTF-16 span [Start, End) in the encoded text (empty for the virtual word-start
/// marker and for bos/eos).</summary>
public readonly record struct BpeToken(int Id, int Start, int End);

/// <summary>Byte-fallback BPE from a Hugging Face tokenizer.json of the Gemma family (as the NER model ships it):
/// added tokens split the text first; each other piece gets its spaces replaced by U+2581, a U+2581 in front when it
/// does not start with one, and is split before every U+2581 (Metaspace, prepend "always", split); each word is
/// merged by merge rank from single characters; a character missing from the vocabulary becomes its UTF-8 bytes as
/// &lt;0xXX&gt; tokens. Matches the Rust tokenizers library on the cases in the worker tests.</summary>
public sealed class BpeTokenizer
{
    private const char Meta = '▁';
    private readonly Dictionary<string, int> _vocab;
    private readonly Dictionary<long, (int Rank, int Id)> _merges;
    private readonly string[] _added;           // longest first
    private readonly Dictionary<string, int> _addedIds;
    private readonly int[] _byteIds = new int[256];

    public int BosId { get; }
    public int EosId { get; }
    public int UnkId { get; }

    private BpeTokenizer(Dictionary<string, int> vocab, Dictionary<long, (int, int)> merges, Dictionary<string, int> added, int bos, int eos, int unk)
    {
        _vocab = vocab; _merges = merges; _addedIds = added;
        _added = added.Keys.Where(k => k.Length > 0).OrderByDescending(k => k.Length).ToArray();
        BosId = bos; EosId = eos; UnkId = unk;
        for (int b = 0; b < 256; b++) _byteIds[b] = vocab.TryGetValue($"<0x{b:X2}>", out var id) ? id : unk;
    }

    public static BpeTokenizer Load(string path)
    {
        using var doc = JsonDocument.Parse(File.ReadAllBytes(path));
        var root = doc.RootElement;
        var model = root.GetProperty("model");
        if (model.GetProperty("type").GetString() != "BPE") throw new InvalidDataException("tokenizer.json: not a BPE model");
        var vocab = new Dictionary<string, int>(StringComparer.Ordinal);
        foreach (var p in model.GetProperty("vocab").EnumerateObject()) vocab[p.Name] = p.Value.GetInt32();
        var added = new Dictionary<string, int>(StringComparer.Ordinal);
        if (root.TryGetProperty("added_tokens", out var at))
            foreach (var a in at.EnumerateArray())
            {
                var content = a.GetProperty("content").GetString() ?? "";
                int id = a.GetProperty("id").GetInt32();
                added[content] = id;
                vocab.TryAdd(content, id);
            }
        var merges = new Dictionary<long, (int, int)>();
        int rank = 0;
        foreach (var m in model.GetProperty("merges").EnumerateArray())
        {
            string a, b;
            if (m.ValueKind == JsonValueKind.Array) { a = m[0].GetString()!; b = m[1].GetString()!; }
            else
            {
                var s = m.GetString()!;
                int sp = s.IndexOf(' ');
                a = s[..sp]; b = s[(sp + 1)..];
            }
            if (vocab.TryGetValue(a, out var ia) && vocab.TryGetValue(b, out var ib) && vocab.TryGetValue(a + b, out var ic))
                merges.TryAdd(((long)ia << 32) | (uint)ib, (rank, ic));
            rank++;
        }
        int Id(string t, int fallback) => vocab.TryGetValue(t, out var i) ? i : fallback;
        return new BpeTokenizer(vocab, merges, added, Id("<bos>", 2), Id("<eos>", 1), Id("<unk>", 3));
    }

    /// <summary>Tokens of text without bos/eos.</summary>
    public List<BpeToken> Encode(string text)
    {
        var tokens = new List<BpeToken>();
        int pos = 0, segStart = 0;
        while (pos < text.Length)
        {
            string? hit = null;
            foreach (var a in _added)
                if (string.CompareOrdinal(text, pos, a, 0, a.Length) == 0) { hit = a; break; }
            if (hit is null) { pos++; continue; }
            EncodeSegment(text, segStart, pos, tokens);
            tokens.Add(new BpeToken(_addedIds[hit], pos, pos + hit.Length));
            pos += hit.Length;
            segStart = pos;
        }
        EncodeSegment(text, segStart, text.Length, tokens);
        return tokens;
    }

    private void EncodeSegment(string text, int start, int end, List<BpeToken> tokens)
    {
        if (end <= start) return;
        // Words: split before each space (U+2581 after normalisation); the first word gets a virtual marker.
        int wordStart = start;
        bool virtualMeta = text[start] != ' ' && text[start] != Meta;
        for (int i = start + 1; i <= end; i++)
        {
            if (i < end && text[i] != ' ' && text[i] != Meta) continue;
            EncodeWord(text, wordStart, i, virtualMeta, tokens);
            virtualMeta = false;
            wordStart = i;
        }
    }

    private struct Sym { public int Id, Start, End; }

    private void EncodeWord(string text, int start, int end, bool virtualMeta, List<BpeToken> tokens)
    {
        var syms = new List<Sym>(end - start + 1);
        if (virtualMeta) syms.Add(new Sym { Id = _vocab.TryGetValue(Meta.ToString(), out var m) ? m : UnkId, Start = start, End = start });
        for (int i = start; i < end;)
        {
            int len = char.IsHighSurrogate(text[i]) && i + 1 < end && char.IsLowSurrogate(text[i + 1]) ? 2 : 1;
            string ch = text[i] == ' ' ? Meta.ToString() : text.Substring(i, len);
            if (_vocab.TryGetValue(ch, out var id)) syms.Add(new Sym { Id = id, Start = i, End = i + len });
            else
                foreach (var b in Encoding.UTF8.GetBytes(ch)) syms.Add(new Sym { Id = _byteIds[b], Start = i, End = i + len });
            i += len;
        }
        Merge(syms);
        foreach (var s in syms) tokens.Add(new BpeToken(s.Id, s.Start, s.End));
    }

    /// <summary>Applies merges lowest rank first, leftmost on ties, until none applies: the same result as rescanning
    /// the list per merge, which cost O(n^2) on a long word without spaces (8 000 chars took 6.5 s) (C9). Symbols form
    /// a linked list; candidate pairs wait in a priority queue keyed (rank, position) and are skipped once either side
    /// has changed.</summary>
    private void Merge(List<Sym> syms)
    {
        int n = syms.Count;
        if (n < 2) return;
        var arr = syms.ToArray();
        var next = new int[n];
        var prev = new int[n];
        var ver = new int[n];
        for (int i = 0; i < n; i++) { next[i] = i + 1 < n ? i + 1 : -1; prev[i] = i - 1; }
        var queue = new PriorityQueue<(int L, int R, int VerL, int VerR, int Id), long>();
        void Push(int l)
        {
            int r = l < 0 ? -1 : next[l];
            if (r < 0 || !_merges.TryGetValue(((long)arr[l].Id << 32) | (uint)arr[r].Id, out var mg)) return;
            queue.Enqueue((l, r, ver[l], ver[r], mg.Id), ((long)mg.Rank << 32) | (uint)l);
        }
        for (int i = 0; i + 1 < n; i++) Push(i);
        while (queue.TryDequeue(out var e, out _))
        {
            if (ver[e.L] != e.VerL || ver[e.R] != e.VerR || next[e.L] != e.R) continue;   // a side merged since
            arr[e.L] = new Sym { Id = e.Id, Start = arr[e.L].Start, End = arr[e.R].End };
            ver[e.L]++;
            ver[e.R] = -1;   // gone
            next[e.L] = next[e.R];
            if (next[e.R] >= 0) prev[next[e.R]] = e.L;
            Push(prev[e.L]);
            Push(e.L);
        }
        syms.Clear();
        for (int i = 0; i >= 0; i = next[i]) syms.Add(arr[i]);   // the first symbol is never merged away
    }
}
