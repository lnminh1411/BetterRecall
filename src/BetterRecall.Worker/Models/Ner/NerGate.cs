// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using BetterRecall.Contracts;
using BetterRecall.Worker.Hosting;
using BetterRecall.Worker.Protocol;

namespace BetterRecall.Worker.Models.Ner;

/// <summary>
/// Runs the name and address detector on OCR lines without ever losing the lines (C9). A missing model, a damaged one
/// (bad tokenizer.json, ONNX load error, label mismatch) or a runtime error returns the lines unchanged with
/// EntitiesMissing, so the agent's pattern redaction (cards, IBANs, birth dates) still runs and it warns that names and
/// addresses are not redacted. After a failure other than "not installed" the model is not tried again for
/// <see cref="Backoff"/>, so a broken folder does not cost a failed load on every keyframe.
/// </summary>
internal sealed class NerGate(Func<long>? nowMs = null)
{
    internal static readonly TimeSpan Backoff = TimeSpan.FromMinutes(10);
    private readonly Func<long> _now = nowMs ?? (() => Environment.TickCount64);
    private long _retryAt = long.MinValue;
    private long _lastLog = long.MinValue;

    /// <summary>Type name of the last failure, or null.</summary>
    public string? LastError { get; private set; }

    /// <summary>ensure loads the model (or throws) and returns its Find. Lines with entities, or the lines as given and
    /// missing = true when the model is not installed or failed.</summary>
    public (OcrLine[] Lines, bool Missing) Apply(OcrLine[] lines, Func<Func<IReadOnlyList<string>, OcrEntity[]?[]>> ensure)
    {
        if (lines.Length == 0) return (lines, false);
        long now = _now();
        if (now < _retryAt) return (lines, true);
        try
        {
            var find = ensure();
            var found = find(lines.Select(l => l.Text).ToList());
            if (found.Length != lines.Length) throw new InvalidDataException("ner result count does not match the lines");
            LastError = null;
            return (lines.Select((l, i) => l with { Entities = found[i] ?? [] }).ToArray(), false);
        }
        catch (ModelNotInstalledException)
        {
            // Cheap to check again on the next frame: the model may be installed meanwhile.
            return (lines, true);
        }
        catch (Exception e) when (e is not OutOfMemoryException)
        {
            _retryAt = now + (long)Backoff.TotalMilliseconds;
            LastError = e.GetType().Name;
            if (_lastLog == long.MinValue || now - _lastLog >= (long)Backoff.TotalMilliseconds)
            {
                _lastLog = now;
                // Type only: a message could quote line text.
                Log.Warn($"ner failed ({e.GetType().Name}): names and addresses not redacted, pattern redaction still runs; retry in {Backoff.TotalMinutes:F0} min");
            }
            return (lines, true);
        }
    }
}
