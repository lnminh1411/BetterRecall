// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using Microsoft.ML.Tokenizers;

namespace BetterRecall.Worker.Models;

/// <summary>SentencePiece tokenizer (Gemma / SigLIP 2 vocabulary) through Microsoft.ML.Tokenizers, managed code only.</summary>
public sealed class GemmaTokenizer
{
    private readonly SentencePieceTokenizer _sp;
    private readonly TokenizerSpec _spec;

    public GemmaTokenizer(string modelFile, TokenizerSpec spec)
    {
        using var fs = File.OpenRead(modelFile);
        _sp = SentencePieceTokenizer.Create(fs, false, false);
        _spec = spec;
    }

    /// <summary>Ids with BOS/EOS per spec, truncated to <paramref name="maxLen"/> (EOS kept as the last id).</summary>
    public long[] Encode(string text, int maxLen)
    {
        if (_spec.Lowercase) text = text.ToLowerInvariant();
        var body = _sp.EncodeToIds(text, addBeginningOfSentence: false, addEndOfSentence: false);
        int extra = (_spec.AddBos ? 1 : 0) + (_spec.AddEos ? 1 : 0);
        int take = Math.Max(0, Math.Min(body.Count, maxLen - extra));
        var ids = new List<long>(take + extra);
        if (_spec.AddBos) ids.Add(_spec.BosId);
        for (int i = 0; i < take; i++) ids.Add(body[i]);
        if (_spec.AddEos) ids.Add(_spec.EosId);
        return ids.ToArray();
    }

    public long PadId => _spec.PadId;
}
