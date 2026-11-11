// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Globalization;
using System.Text;
using System.Text.RegularExpressions;
using BetterRecall.Contracts;

namespace BetterRecall.Core.Ai;

/// <summary>Scrubs text that may leave the PC in a chat. Batch 3 can replace it with its OCR-aware redactor.</summary>
public interface ITextRedactor
{
    string Redact(string text);
}

/// <summary>
/// Conservative pattern scrubber: e-mail addresses, URL paths and queries, labelled secrets ("password: x"),
/// token-like strings, and any run of 8 or more digits (cards, phones, IBANs, account and ID numbers) except plain
/// dates. Over-redacting is the intended failure mode.
/// </summary>
public sealed partial class BasicTextRedactor : ITextRedactor
{
    public const string Mask = "[redacted]";

    public string Redact(string text)
    {
        if (string.IsNullOrEmpty(text)) return "";
        var t = Labelled().Replace(text, m => m.Groups[1].Value + Mask);
        t = Email().Replace(t, Mask);
        t = Url().Replace(t, m => m.Groups[1].Value + (m.Groups[2].Value.Length > 1 ? "/" + Mask : ""));
        t = Iban().Replace(t, Mask);
        t = Digits().Replace(t, m => IsDate(m.Value) || CountDigits(m.Value) < 8 ? m.Value : Mask);
        t = TokenLike().Replace(t, Mask);
        return t;
    }

    private static int CountDigits(string s) => s.Count(char.IsAsciiDigit);

    private static bool IsDate(string s) => DateLike().IsMatch(s.Trim());

    [GeneratedRegex(@"(?i)\b((?:pass(?:word|code|phrase)?|pwd|pin|otp|cvv|cvc|token|secret|api[ _-]?key)\s*[:=]\s*)\S+")]
    private static partial Regex Labelled();

    [GeneratedRegex(@"[A-Za-z0-9._%+\-]+@[A-Za-z0-9.\-]+\.[A-Za-z]{2,}")]
    private static partial Regex Email();

    /// <summary>Group 1 keeps scheme and host; group 2 is the path, query and fragment that are dropped.</summary>
    [GeneratedRegex(@"(?i)\b((?:https?|ftp)://[^\s/?#]+)([/?#][^\s]*)?")]
    private static partial Regex Url();

    [GeneratedRegex(@"\b[A-Z]{2}\d{2}(?:[ ]?[A-Z0-9]{4}){2,7}(?:[ ]?[A-Z0-9]{1,4})?\b")]
    private static partial Regex Iban();

    /// <summary>Digit groups joined by single spaces, dashes, dots, slashes or brackets, with an optional leading +.</summary>
    [GeneratedRegex(@"\+?\(?\d(?:[\d]|[ \-./()](?=\d))*\d")]
    private static partial Regex Digits();

    [GeneratedRegex(@"^(?:\d{4}[-./]\d{1,2}[-./]\d{1,2}|\d{1,2}[-./]\d{1,2}[-./]\d{4})$")]
    private static partial Regex DateLike();

    /// <summary>24+ characters of letters, digits, _ or - with both a letter and a digit: API keys, session tokens.</summary>
    [GeneratedRegex(@"\b(?=[A-Za-z0-9_\-]*\d)(?=[A-Za-z0-9_\-]*[A-Za-z])[A-Za-z0-9_\-]{24,}\b")]
    private static partial Regex TokenLike();
}

/// <summary>One capture prepared for a chat: the exact text and optional redacted image that may be sent.</summary>
public sealed record SharedHit(string Ref, SearchHit Hit, string Text, OutgoingImage? Image);

/// <summary>What one search tool call would send. ImagesUnavailable: images were asked for (ai.share textImages) but at
/// least one could not be checked (no image redactor, OCR off or not installed, OCR failed), so those captures go as
/// text only.</summary>
public sealed record SharePlan(string Share, IReadOnlyList<SharedHit> Hits, bool ImagesUnavailable);

/// <summary>
/// The only place that decides what a chat sends about captures. Fails closed: an unknown share level counts as
/// metadata; images go only through IDescribeRedactor with RedactsImages and only when the result says Redacted (OCR
/// checked it); a redactor that fails leaves that capture as text only. Titles and text come only from the store
/// (captures.title and captures.text, already redacted when captured) and go through the text redactor again;
/// nothing is read from the image for text.
/// </summary>
public static class ChatShareGate
{
    public const int SnippetChars = 400;
    public const int ImageWidth = 768;
    public const string ImagesUnavailableNote = "Some images were not shared: images are sent only after on-screen text reading has checked them, and it is off, not installed or failed. Only text was sent for those captures. Turn on on-screen text reading (Settings > OCR) to share images.";

    public static async Task<SharePlan> BuildAsync(IReadOnlyList<SearchHit> hits, string share, int maxHits, int maxImages,
        Func<string, CaptureDetail?> detail, Func<string, int, CancellationToken, Task<ThumbData?>> thumb,
        IDescribeRedactor? imageRedactor, ITextRedactor textRedactor, CancellationToken ct)
    {
        int rank = Math.Max(0, AiShare.Rank(share));
        var level = AiShare.All.First(a => AiShare.Rank(a) == rank);
        bool wantImages = rank >= 2 && maxImages > 0;
        bool canImages = wantImages && imageRedactor is { RedactsImages: true };
        var list = new List<SharedHit>();
        int images = 0;
        bool unchecked_ = wantImages && !canImages;
        foreach (var h in hits.Take(Math.Clamp(maxHits, 1, 20)))
        {
            var refId = "S" + (list.Count + 1).ToString(CultureInfo.InvariantCulture);
            var d = rank >= 1 ? detail(h.Id) : null;
            var text = Describe(refId, h, d, rank, textRedactor);
            OutgoingImage? image = null;
            if (canImages && images < maxImages && h.Kind != CaptureKind.Audio)
            {
                var t = await thumb(h.Id, ImageWidth, ct).ConfigureAwait(false);
                if (t is { Bytes.Length: > 0 })
                {
                    OutgoingImage? red;
                    try { red = await imageRedactor!.RedactAsync(h.Id, new OutgoingImage(t.Bytes, t.Mime, t.Width, t.Height, false), ct).ConfigureAwait(false); }
                    catch (Exception ex) when (ex is not OperationCanceledException || !ct.IsCancellationRequested)
                    {
                        Log.Warn($"chat image redaction failed, sending text only ({ex.GetType().Name})");
                        red = null;
                    }
                    if (red is { Redacted: true }) { image = red; images++; }
                    else unchecked_ = true;   // not checked (OCR off, not installed, failed): text only for this capture
                }
            }
            list.Add(new SharedHit(refId, h, text, image));
        }
        return new SharePlan(level, list, unchecked_);
    }

    /// <summary>One capture as the model sees it. metadata: time, kind, app, site. text: plus redacted title and snippet.</summary>
    public static string Describe(string refId, SearchHit h, CaptureDetail? d, int rank, ITextRedactor r)
    {
        var sb = new StringBuilder();
        sb.Append('[').Append(refId).Append("] ")
          .Append(h.Ts.ToLocalTime().ToString("yyyy-MM-dd HH:mm zzz", CultureInfo.InvariantCulture))
          .Append(" | ").Append(KindName(h.Kind))
          .Append(" | app: ").Append(h.App);
        if (h.Site.Length > 0) sb.Append(" | site: ").Append(h.Site);
        if (h.ReplayOffsetMs is { } off) sb.Append(" | replay at ").Append(TimeSpan.FromMilliseconds(off).ToString(@"m\:ss", CultureInfo.InvariantCulture));
        if (rank >= 1)
        {
            var title = r.Redact(Clean(h.Title));
            if (title.Length > 0) sb.Append("\n  title: ").Append(Cut(title, 200));
            var body = d?.Text is { Length: > 0 } full && full != h.Title ? full : h.Snippet;
            body = r.Redact(Clean(body ?? ""));
            if (body.Length > 0) sb.Append("\n  text: ").Append(Cut(body, SnippetChars));
        }
        return sb.ToString();
    }

    /// <summary>The tool result text: the shared captures, or a plain note when none matched or the user removed all.</summary>
    public static string ToolText(SharePlan plan, bool declined = false)
    {
        if (declined) return "The user did not allow sharing captures for this question. Answer without them, or say what you would need.";
        if (plan.Hits.Count == 0) return "No matching captures were found (or none were shared).";
        var sb = new StringBuilder("Captures from the user's history (share level ").Append(plan.Share).Append("). Cite them as [S1], [S2]:\n");
        foreach (var h in plan.Hits) sb.Append(h.Text).Append('\n');
        if (plan.ImagesUnavailable) sb.Append(ImagesUnavailableNote).Append('\n');
        return sb.ToString().TrimEnd();
    }

    public static ChatSource Source(SearchHit h) => new(h.Id, h.Ts, h.Kind, h.App, h.Title, h.Site, h.ReplayId, h.ReplayOffsetMs);

    public static ChatApprovalItem Item(SharedHit s, int rank, ITextRedactor r) =>
        new(s.Hit.Id, s.Hit.Ts, s.Hit.Kind, s.Hit.App, rank >= 1 ? r.Redact(Clean(s.Hit.Title)) : "", s.Hit.Site, s.Text, s.Image?.Mime, s.Image?.Bytes);

    private static string KindName(CaptureKind k) => k switch { CaptureKind.Audio => "speech transcript", CaptureKind.Replay => "replay frame", _ => "screen" };

    private static string Clean(string s) => string.Join(' ', s.Split((char[]?)null, StringSplitOptions.RemoveEmptyEntries));

    private static string Cut(string s, int n) => s.Length <= n ? s : s[..n].TrimEnd() + "...";
}
