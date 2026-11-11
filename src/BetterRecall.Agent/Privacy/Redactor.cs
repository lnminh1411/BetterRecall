// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Text;
using System.Text.RegularExpressions;
using BetterRecall.Contracts;

namespace BetterRecall.Agent.Privacy;

/// <summary>Rectangle in keyframe pixels.</summary>
internal readonly record struct Box(int X, int Y, int W, int H)
{
    public int Right => X + W;
    public int Bottom => Y + H;
    public long Area => (long)Math.Max(0, W) * Math.Max(0, H);

    public long Overlap(Box o)
    {
        int w = Math.Min(Right, o.Right) - Math.Max(X, o.X), h = Math.Min(Bottom, o.Bottom) - Math.Max(Y, o.Y);
        return w > 0 && h > 0 ? (long)w * h : 0;
    }
}

/// <summary>Which detectors run (privacy.redact*). Text detectors need OCR lines; Passwords also covers UI
/// Automation password fields, which need no OCR.</summary>
internal sealed record RedactionOptions(bool Passwords, bool Cards, bool Ids, bool BirthDates, bool Emails, bool Phones, bool Names, bool Addresses)
{
    public static RedactionOptions From(PrivacySettings p) => new(p.RedactPasswords, p.RedactCards, p.RedactIds, p.RedactBirthDates,
        p.RedactEmails, p.RedactPhones, p.RedactNames, p.RedactAddresses);

    public static readonly RedactionOptions None = new(false, false, false, false, false, false, false, false);

    public bool AnyText => Passwords || Cards || Ids || BirthDates || Emails || Phones || Names || Addresses;
    public bool Entities => Names || Addresses;
}

/// <summary>Boxes to black out (keyframe pixels), the stored text (lines in reading order, redacted characters left
/// out), the title with string detectors applied, and the number of redacted spans.</summary>
internal sealed record RedactionResult(Box[] Boxes, string Text, string Title, int Spans);

/// <summary>Finds sensitive text in OCR lines and returns what to black out and what text to keep. Pure.
/// Detectors: password labels (the rest of the line, else the field to the right or below), API-key-like tokens,
/// payment cards (Luhn), IBAN (mod 97), US SSN and numbers after national-id keywords, dates near birth keywords
/// (same line or the next field), emails, phone numbers, and NER person/address spans from the worker.</summary>
internal static partial class Redactor
{
    public const int Pad = 2;

    public static RedactionResult Apply(IReadOnlyList<OcrLine> lines, IReadOnlyList<Box> secretFields, RedactionOptions o, string title = "")
    {
        var ordered = ReadingOrder(lines);
        var spans = new List<(int S, int E)>[ordered.Count];
        for (int i = 0; i < ordered.Count; i++) spans[i] = [];
        for (int i = 0; i < ordered.Count; i++)
        {
            var l = ordered[i];
            string t = l.Text;
            spans[i].AddRange(StringSpans(t, o));
            if (o.Passwords)
                foreach (var m in PasswordLabel().EnumerateMatches(t))
                {
                    int s = SkipSeparators(t, m.Index + m.Length);
                    bool onlyPunctuation = t.AsSpan(s).Trim().ToString().All(char.IsPunctuation);
                    if (!onlyPunctuation) spans[i].Add((s, t.Length));
                    // "Forgot password?" is a link, not a label.
                    else if (!t.AsSpan(s).Contains('?') && Following(ordered, i) is int j) spans[j].Add((0, ordered[j].Text.Length));
                }
            if (o.Ids)
                foreach (var m in IdLabel().EnumerateMatches(t))
                {
                    int s = SkipSeparators(t, m.Index + m.Length);
                    var v = IdValue().Match(t, s);
                    if (v.Success && Digits(v.Value) >= 5) spans[i].Add((v.Index, v.Index + v.Length));
                    else if (s >= t.Length && Following(ordered, i) is int j && IdValue().Match(ordered[j].Text) is { Success: true } nv && Digits(nv.Value) >= 5)
                        spans[j].Add((nv.Index, nv.Index + nv.Length));
                }
            if (o.BirthDates && BirthLabel().Match(t) is { Success: true } bm)
            {
                bool found = false;
                foreach (var d in Dates(t, bm.Index + bm.Length)) { spans[i].Add(d); found = true; }
                if (!found) foreach (var d in Dates(t, 0)) { spans[i].Add(d); found = true; }
                if (!found && Following(ordered, i) is int j)
                    foreach (var d in Dates(ordered[j].Text, 0)) spans[j].Add(d);
            }
            if (o.Entities && l.Entities is { } ents)
                foreach (var e in ents)
                {
                    if (!(e.Label == "person" ? o.Names : e.Label is "address" or "location" && o.Addresses)) continue;
                    int s = Math.Clamp(e.Start, 0, t.Length), en = Math.Clamp(e.Start + e.Length, s, t.Length);
                    while (s < en && char.IsWhiteSpace(t[s])) s++;
                    while (en > s && char.IsWhiteSpace(t[en - 1])) en--;
                    if (en > s) spans[i].Add((s, en));
                }
            if (o.Passwords)
                foreach (var f in secretFields)
                    for (int c = 0; c < t.Length; c++)
                        if (CharBox(l, c) is { } cb && f.Overlap(cb) * 2 >= cb.Area && cb.Area > 0) spans[i].Add((c, c + 1));
        }

        var boxes = new List<Box>();
        if (o.Passwords) boxes.AddRange(secretFields.Where(f => f.W > 0 && f.H > 0));
        var text = new StringBuilder();
        int count = 0;
        for (int i = 0; i < ordered.Count; i++)
        {
            var l = ordered[i];
            var merged = Merge(spans[i], l.Text.Length);
            count += merged.Count;
            foreach (var (s, e) in merged) boxes.Add(SpanBox(l, s, e));
            var kept = Remove(l.Text, merged);
            if (kept.Length == 0) continue;
            if (text.Length > 0) text.Append('\n');
            text.Append(kept);
        }
        var titleSpans = Merge(StringSpans(title, o).ToList(), title.Length);
        return new RedactionResult(boxes.ToArray(), text.ToString(), Remove(title, titleSpans), count + titleSpans.Count);
    }

    /// <summary>Spans found from the string alone (no layout): cards, IBAN, SSN, secret tokens, emails, phones.</summary>
    internal static IEnumerable<(int S, int E)> StringSpans(string t, RedactionOptions o)
    {
        if (string.IsNullOrEmpty(t)) yield break;
        var taken = new List<(int, int)>();
        if (o.Cards)
            foreach (Match m in CardCandidate().Matches(t))
                if (CardSpan(m.Value) is int len) { var sp = (m.Index, m.Index + len); taken.Add(sp); yield return sp; }
        if (o.Ids)
        {
            foreach (Match m in IbanCandidate().Matches(t))
                if (IbanSpan(m.Value) is int len) { var sp = (m.Index, m.Index + len); taken.Add(sp); yield return sp; }
            foreach (Match m in Ssn().Matches(t)) { var sp = (m.Index, m.Index + m.Length); taken.Add(sp); yield return sp; }
        }
        if (o.Passwords)
            foreach (Match m in SecretToken().Matches(t)) yield return (m.Index, m.Index + m.Length);
        if (o.Emails)
            foreach (Match m in Email().Matches(t)) { var sp = (m.Index, m.Index + m.Length); taken.Add(sp); yield return sp; }
        if (o.Phones)
        {
            bool keyword = PhoneLabel().IsMatch(t);
            foreach (Match m in PhoneCandidate().Matches(t))
            {
                var v = m.Value.TrimEnd(' ', '.', '-');
                int d = Digits(v);
                if (d is < 9 or > 15) continue;
                if (taken.Any(x => x.Item1 < m.Index + v.Length && m.Index < x.Item2)) continue;
                if (DateRegexes().Any(r => r.IsMatch(v))) continue;
                bool shaped = v.StartsWith('+') || v.StartsWith('(') || v.StartsWith('0') || NanpPhone().IsMatch(v);
                if (shaped || keyword) yield return (m.Index, m.Index + v.Length);
            }
        }
    }

    // ---------------- layout ----------------

    /// <summary>Lines sorted into rows (vertical centres within half a line height) top to bottom, each row left to
    /// right.</summary>
    internal static List<OcrLine> ReadingOrder(IReadOnlyList<OcrLine> lines)
    {
        var byY = lines.OrderBy(l => l.Y + l.H / 2.0).ToList();
        var rows = new List<List<OcrLine>>();
        foreach (var l in byY)
        {
            double c = l.Y + l.H / 2.0;
            var row = rows.LastOrDefault();
            if (row is not null && Math.Abs(c - row.Average(r => r.Y + r.H / 2.0)) <= Math.Max(4, row.Min(r => r.H) / 2.0)) row.Add(l);
            else rows.Add([l]);
        }
        return rows.SelectMany(r => r.OrderBy(l => l.X)).ToList();
    }

    /// <summary>The field that follows a label line: the nearest line to its right on the same row (gap at most
    /// 12 line heights), else the nearest line below it (top within 2 line heights of the label's bottom) whose left
    /// edge is within 4 line heights of the label's left edge or that overlaps it horizontally.</summary>
    internal static int? Following(IReadOnlyList<OcrLine> lines, int i)
    {
        var a = lines[i];
        int? best = null; double bestD = double.MaxValue;
        for (int j = 0; j < lines.Count; j++)
        {
            if (j == i) continue;
            var b = lines[j];
            int vo = Math.Min(a.Y + a.H, b.Y + b.H) - Math.Max(a.Y, b.Y);
            if (vo * 2 >= Math.Min(a.H, b.H) && b.X >= a.X + a.W - 2 && b.X - (a.X + a.W) <= 12 * a.H)
            {
                double d = b.X - (a.X + a.W);
                if (d < bestD) { bestD = d; best = j; }
            }
        }
        if (best is not null) return best;
        for (int j = 0; j < lines.Count; j++)
        {
            if (j == i) continue;
            var b = lines[j];
            int gap = b.Y - (a.Y + a.H);
            bool below = b.Y + b.H / 2 > a.Y + a.H && gap <= 2 * a.H;
            bool aligned = Math.Abs(b.X - a.X) <= 4 * a.H || Math.Min(a.X + a.W, b.X + b.W) > Math.Max(a.X, b.X);
            if (below && aligned)
            {
                double d = Math.Max(0, gap) * 4 + Math.Abs(b.X - a.X);
                if (d < bestD) { bestD = d; best = j; }
            }
        }
        return best;
    }

    private static Box? CharBox(OcrLine l, int c)
    {
        if (l.Cuts.Length != l.Text.Length + 1) return null;
        return new Box(l.Cuts[c], l.Y, Math.Max(1, l.Cuts[c + 1] - l.Cuts[c]), l.H);
    }

    /// <summary>Box over characters [s, e) of a line, padded; the whole line when its cuts are unusable.</summary>
    internal static Box SpanBox(OcrLine l, int s, int e)
    {
        int x0 = l.X, x1 = l.X + l.W;
        if (l.Cuts.Length == l.Text.Length + 1 && e > s) { x0 = l.Cuts[s]; x1 = l.Cuts[e]; }
        return new Box(x0 - Pad, l.Y - Pad, Math.Max(1, x1 - x0) + 2 * Pad, l.H + 2 * Pad);
    }

    internal static List<(int S, int E)> Merge(List<(int S, int E)> spans, int len)
    {
        var sorted = spans.Select(x => (S: Math.Clamp(x.S, 0, len), E: Math.Clamp(x.E, 0, len))).Where(x => x.E > x.S).OrderBy(x => x.S).ToList();
        var outp = new List<(int S, int E)>();
        foreach (var x in sorted)
        {
            if (outp.Count > 0 && x.S <= outp[^1].E) outp[^1] = (outp[^1].S, Math.Max(outp[^1].E, x.E));
            else outp.Add(x);
        }
        return outp;
    }

    /// <summary>Text with the spans left out; runs of spaces collapse to one, ends trimmed.</summary>
    internal static string Remove(string text, List<(int S, int E)> spans)
    {
        if (spans.Count == 0) return text.Trim();
        var sb = new StringBuilder(text.Length);
        int at = 0;
        foreach (var (s, e) in spans) { sb.Append(text, at, s - at); sb.Append(' '); at = e; }
        sb.Append(text, at, text.Length - at);
        return MultiSpace().Replace(sb.ToString(), " ").Trim();
    }

    private static int SkipSeparators(string t, int i)
    {
        while (i < t.Length && (char.IsWhiteSpace(t[i]) || t[i] is ':' or '=' or '：' or '-' or '–' or '.' or '#')) i++;
        return i;
    }

    private static int Digits(string s) => s.Count(char.IsAsciiDigit);

    // ---------------- checksums ----------------

    /// <summary>Candidate length when it is a card number (13-19 digits, starting with 2-6, Luhn), else null. No
    /// shorter prefixes are tried: one random number in ten passes Luhn, so trimming would flag most long numbers.</summary>
    internal static int? CardSpan(string candidate)
    {
        var digits = candidate.Where(char.IsAsciiDigit).ToArray();
        return digits.Length is >= 13 and <= 19 && digits[0] is >= '2' and <= '6' && Luhn(digits) ? candidate.Length : null;
    }

    private static readonly HashSet<string> IbanCountries =
    [
        "AD", "AE", "AL", "AT", "AZ", "BA", "BE", "BG", "BH", "BI", "BR", "BY", "CH", "CR", "CY", "CZ", "DE", "DJ", "DK", "DO", "EE", "EG",
        "ES", "FI", "FK", "FO", "FR", "GB", "GE", "GI", "GL", "GR", "GT", "HN", "HR", "HU", "IE", "IL", "IQ", "IS", "IT", "JO", "KW", "KZ",
        "LB", "LC", "LI", "LT", "LU", "LV", "LY", "MC", "MD", "ME", "MK", "MN", "MR", "MT", "MU", "NI", "NL", "NO", "OM", "PK", "PL", "PS",
        "PT", "QA", "RO", "RS", "RU", "SA", "SC", "SD", "SE", "SI", "SK", "SM", "SO", "ST", "SV", "TL", "TN", "TR", "UA", "VA", "VG", "XK", "YE",
    ];

    internal static bool Luhn(ReadOnlySpan<char> digits)
    {
        int sum = 0;
        bool dbl = false;
        for (int i = digits.Length - 1; i >= 0; i--)
        {
            int d = digits[i] - '0';
            if (dbl) { d *= 2; if (d > 9) d -= 9; }
            sum += d;
            dbl = !dbl;
        }
        return sum % 10 == 0;
    }

    /// <summary>Length of the longest prefix of an IBAN candidate that ends at the candidate's end or before a space
    /// (IBANs are written in groups), has an IBAN country code, 15 to 34 characters and passes mod 97; else null.</summary>
    internal static int? IbanSpan(string candidate)
    {
        if (candidate.Length < 2 || !IbanCountries.Contains(candidate[..2])) return null;
        for (int end = candidate.Length; end > 0; end--)
        {
            if (candidate[end - 1] == ' ' || end < candidate.Length && candidate[end] != ' ') continue;
            var compact = candidate[..end].Replace(" ", "");
            if (compact.Length < 15) return null;
            if (compact.Length > 34) continue;
            if (Mod97(compact) == 1) return end;
        }
        return null;
    }

    private static int Mod97(string iban)
    {
        var s = iban[4..] + iban[..4];
        int r = 0;
        foreach (char ch in s)
        {
            if (char.IsAsciiDigit(ch)) r = (r * 10 + (ch - '0')) % 97;
            else if (char.IsAsciiLetterUpper(ch)) { int v = ch - 'A' + 10; r = (r * 100 + v) % 97; }
            else return -1;
        }
        return r;
    }

    // ---------------- dates ----------------

    private static IEnumerable<Regex> DateRegexes() => [DateNumeric(), DateIso(), DateDayMonth(), DateMonthDay(), DateCjk(), DateVi()];

    internal static IEnumerable<(int S, int E)> Dates(string t, int from)
    {
        foreach (var r in DateRegexes())
            foreach (Match m in r.Matches(t))
                if (m.Index >= from) yield return (m.Index, m.Index + m.Length);
    }

    // ---------------- patterns ----------------

    [GeneratedRegex(@"(?<!\d)\d(?:[ \-]?\d){12,18}(?!\d)")]
    private static partial Regex CardCandidate();

    [GeneratedRegex(@"(?<![A-Za-z0-9])[A-Z]{2}\d{2}(?:[ ]?[A-Z0-9]){11,32}")]
    private static partial Regex IbanCandidate();

    [GeneratedRegex(@"(?<!\d)\d{3}-\d{2}-\d{4}(?!\d)")]
    private static partial Regex Ssn();

    [GeneratedRegex(@"[\p{L}\p{N}._%+\-]+@[\p{L}\p{N}\-]+(?:\.[\p{L}\p{N}\-]+)*\.\p{L}{2,}")]
    private static partial Regex Email();

    [GeneratedRegex(@"(?<![\p{L}\p{N}])(?:\+|00)?\(?\d[\d ().\-]{6,}\d(?![\p{L}\p{N}])")]
    private static partial Regex PhoneCandidate();

    [GeneratedRegex(@"^\(?\d{3}\)?[ .\-]\d{3}[ .\-]\d{4}$")]
    private static partial Regex NanpPhone();

    [GeneratedRegex(@"(?:sk-[A-Za-z0-9_\-]{20,}|gh[pousr]_[A-Za-z0-9]{30,}|github_pat_[A-Za-z0-9_]{30,}|xox[abprs]-[A-Za-z0-9\-]{10,}|AKIA[0-9A-Z]{16}|AIza[0-9A-Za-z_\-]{35}|eyJ[A-Za-z0-9_\-]{10,}\.[A-Za-z0-9_\-]{10,}\.[A-Za-z0-9_\-]{10,})")]
    private static partial Regex SecretToken();

    [GeneratedRegex(@"(?<![\p{L}\p{N}])(?:password|passwort|passwd|pwd|passcode|pass phrase|passphrase|pin code|pin(?=\s*[:=：])|m[ậa]t kh[ẩa]u|kennwort|mot de passe|contrase[ñn]a|senha|wachtwoord|has[łl]o|пароль|parola|l[öo]senord|adgangskode|passord|salasana|heslo|jelsz[óo]|κωδικός|[şs]ifre|api[ _\-]?key|secret key|client secret|access token|auth token|api token|bearer|private key|recovery (?:code|phrase|key)|seed phrase|mnemonic|one-time code|verification code|otp|cvv|cvc|cvv2|security code|m[ãa] otp|m[ãa] x[áa]c (?:nh[ậa]n|th[ựu]c))(?![\p{L}\p{N}])|パスワード|暗証番号|密码|密碼|口令|비밀번호|암호|รหัสผ่าน|كلمة (?:المرور|السر)|סיסמה|पासवर्ड", RegexOptions.IgnoreCase)]
    private static partial Regex PasswordLabel();

    [GeneratedRegex(@"(?<![\p{L}\p{N}])(?:ssn|social security(?: number| no\.?)?|national (?:id|insurance)(?: number| no\.?)?|id (?:number|no\.?|card)|identity (?:card|number)|passport(?: number| no\.?)?|driver'?s licen[cs]e(?: number)?|driving licen[cs]e|tax id|taxpayer id(?:entification number)?|cccd|cmnd|c[ăa]n c[ưu][ớo]c(?: c[ôo]ng d[âa]n)?|s[ốo] đ[ịi]nh danh|m[ãa] s[ốo] thu[ếe]|personalausweis(?:nummer)?|ausweisnummer|steuer-?id|steuernummer|sozialversicherungsnummer|num[ée]ro de s[ée]curit[ée] sociale|dni|nie|nif|cpf|codice fiscale|pesel|bsn|burgerservicenummer|personnummer|cpr|henkil[öo]tunnus|aadhaar|nric|mykad)(?![\p{L}\p{N}])|身份证(?:号码?)?|身份證(?:號碼?)?|マイナンバー|個人番号|주민등록번호|여권번호|เลขประจำตัวประชาชน", RegexOptions.IgnoreCase)]
    private static partial Regex IdLabel();

    // Id value: letters, digits and . - / ; a space only between digits ("001 099 012 345").
    [GeneratedRegex(@"[A-Z0-9](?:[A-Z0-9.\-/]|(?<=\d) (?=\d)){3,}[A-Z0-9]", RegexOptions.IgnoreCase)]
    private static partial Regex IdValue();

    [GeneratedRegex(@"(?<![\p{L}\p{N}])(?:date of birth|birth ?date|birthday|born|dob|d\.o\.b\.?|ng[àa]y sinh|sinh ng[àa]y|geburtsdatum|geboren|geb\.|date de naissance|n[ée]e? le|fecha de nacimiento|data de nascimento|data di nascita|дата рождения|data urodzenia|geboortedatum|f[öo]delsedatum|f[øo]dselsdato|syntym[äa]aika|datum narozen[íi]|sz[üu]let[ée]si (?:id[őo]|d[áa]tum)|do[ğg]um tarihi)(?![\p{L}\p{N}])|生年月日|出生日期|出生年月|生日|생년월일|วันเกิด|تاريخ الميلاد|תאריך לידה|जन्म तिथि", RegexOptions.IgnoreCase)]
    private static partial Regex BirthLabel();

    [GeneratedRegex(@"(?<![\p{L}\p{N}])(?:phone|tel|telephone|mobile|cell|fax|đi[ệe]n tho[ạa]i|s[đd]t|đt|telefon|t[ée]l[ée]phone|tel[ée]fono|telefone|телефон|電話|电话|手机|전화|휴대폰)(?![\p{L}\p{N}])", RegexOptions.IgnoreCase)]
    private static partial Regex PhoneLabel();

    [GeneratedRegex(@"(?<!\d)\d{1,2}[./\-]\d{1,2}[./\-](?:\d{4}|\d{2})(?!\d)")]
    private static partial Regex DateNumeric();

    [GeneratedRegex(@"(?<!\d)\d{4}[./\-]\d{1,2}[./\-]\d{1,2}(?!\d)")]
    private static partial Regex DateIso();

    [GeneratedRegex(@"(?<!\d)\d{1,2}\.?\s+\p{L}{3,12}\.?,?\s+\d{4}(?!\d)")]
    private static partial Regex DateDayMonth();

    [GeneratedRegex(@"(?<![\p{L}])\p{L}{3,12}\.?\s+\d{1,2}(?:st|nd|rd|th)?,?\s+\d{4}(?!\d)")]
    private static partial Regex DateMonthDay();

    [GeneratedRegex(@"\d{4}\s*[年년]\s*\d{1,2}\s*[月월]\s*\d{1,2}\s*[日일]")]
    private static partial Regex DateCjk();

    [GeneratedRegex(@"(?:ng[àa]y\s+)?\d{1,2}\s+th[áa]ng\s+\d{1,2},?\s+(?:n[ăa]m\s+)?\d{4}", RegexOptions.IgnoreCase)]
    private static partial Regex DateVi();

    [GeneratedRegex(@" {2,}")]
    private static partial Regex MultiSpace();
}
