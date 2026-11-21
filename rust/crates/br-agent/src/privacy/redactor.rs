// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Sensitive text and pixel-box redaction over OCR lines (`Privacy\Redactor.cs`).

use std::collections::HashSet;
use std::sync::OnceLock;

use br_contracts::{OcrLine, PrivacySettings};
use fancy_regex::Regex;

/// Rectangle in keyframe pixels (`Privacy.Box`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PixelBox {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl PixelBox {
    pub const fn new(x: i32, y: i32, w: i32, h: i32) -> Self {
        Self { x, y, w, h }
    }

    pub const fn right(self) -> i32 {
        self.x + self.w
    }

    pub const fn bottom(self) -> i32 {
        self.y + self.h
    }

    pub const fn area(self) -> i64 {
        let w = if self.w > 0 { self.w as i64 } else { 0 };
        let h = if self.h > 0 { self.h as i64 } else { 0 };
        w * h
    }

    pub fn overlap(self, other: Self) -> i64 {
        let w = self.right().min(other.right()) - self.x.max(other.x);
        let h = self.bottom().min(other.bottom()) - self.y.max(other.y);
        if w > 0 && h > 0 {
            (w as i64) * (h as i64)
        } else {
            0
        }
    }
}

/// Which detectors run (`privacy.redact*`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RedactionOptions {
    pub passwords: bool,
    pub cards: bool,
    pub ids: bool,
    pub birth_dates: bool,
    pub emails: bool,
    pub phones: bool,
    pub names: bool,
    pub addresses: bool,
}

impl RedactionOptions {
    pub const NONE: Self = Self {
        passwords: false,
        cards: false,
        ids: false,
        birth_dates: false,
        emails: false,
        phones: false,
        names: false,
        addresses: false,
    };

    pub const ALL: Self = Self {
        passwords: true,
        cards: true,
        ids: true,
        birth_dates: true,
        emails: true,
        phones: true,
        names: true,
        addresses: true,
    };

    pub fn from_privacy(p: &PrivacySettings) -> Self {
        Self {
            passwords: p.redact_passwords,
            cards: p.redact_cards,
            ids: p.redact_ids,
            birth_dates: p.redact_birth_dates,
            emails: p.redact_emails,
            phones: p.redact_phones,
            names: p.redact_names,
            addresses: p.redact_addresses,
        }
    }

    pub const fn any_text(self) -> bool {
        self.passwords
            || self.cards
            || self.ids
            || self.birth_dates
            || self.emails
            || self.phones
            || self.names
            || self.addresses
    }

    pub const fn entities(self) -> bool {
        self.names || self.addresses
    }
}

/// Boxes to black out (keyframe pixels), stored text (reading order, redacted chars left out),
/// redacted title, and count of redacted spans.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RedactionResult {
    pub boxes: Vec<PixelBox>,
    pub text: String,
    pub title: String,
    pub spans: usize,
}

/// Pure sensitive-text redactor over OCR lines (`Redactor.cs`).
pub struct Redactor;

impl Redactor {
    pub const PAD: i32 = 2;

    pub fn apply(
        lines: &[OcrLine],
        secret_fields: &[PixelBox],
        o: RedactionOptions,
        title: &str,
    ) -> RedactionResult {
        let ordered = Self::reading_order(lines);
        let mut spans: Vec<Vec<(usize, usize)>> = vec![Vec::new(); ordered.len()];
        let pat = patterns();

        for i in 0..ordered.len() {
            let l = &ordered[i];
            let t = l.text.as_str();
            let c2b = char_byte_map(t);
            let char_len = c2b.len() - 1;

            spans[i].extend(Self::string_spans(t, o));

            if o.passwords {
                for m in pat.password_label.find_iter(t).flatten() {
                    let s = skip_separators(t, &c2b, byte_to_char(&c2b, m.end()));
                    let rest = &t[c2b[s]..];
                    let only_punct = pat.only_punct.is_match(rest.trim()).unwrap_or(false);
                    if !only_punct {
                        spans[i].push((s, char_len));
                    } else if !rest.contains('?')
                        && let Some(j) = Self::following(&ordered, i)
                    {
                        let next_len = ordered[j].text.chars().count();
                        spans[j].push((0, next_len));
                    }
                }
            }

            if o.ids {
                for m in pat.id_label.find_iter(t).flatten() {
                    let s = skip_separators(t, &c2b, byte_to_char(&c2b, m.end()));
                    let v = pat.id_value.find_from_pos(t, c2b[s]).ok().flatten();
                    if let Some(mv) = v
                        && count_digits(mv.as_str()) >= 5
                    {
                        spans[i].push((byte_to_char(&c2b, mv.start()), byte_to_char(&c2b, mv.end())));
                    } else if s >= char_len
                        && let Some(j) = Self::following(&ordered, i)
                    {
                        let nt = ordered[j].text.as_str();
                        if let Ok(Some(nv)) = pat.id_value.find(nt)
                            && count_digits(nv.as_str()) >= 5
                        {
                            let nc2b = char_byte_map(nt);
                            spans[j].push((
                                byte_to_char(&nc2b, nv.start()),
                                byte_to_char(&nc2b, nv.end()),
                            ));
                        }
                    }
                }
            }

            if o.birth_dates
                && let Ok(Some(bm)) = pat.birth_label.find(t)
            {
                let after = byte_to_char(&c2b, bm.end());
                let mut found = false;
                for d in Self::dates(t, after) {
                    spans[i].push(d);
                    found = true;
                }
                if !found {
                    for d in Self::dates(t, 0) {
                        spans[i].push(d);
                        found = true;
                    }
                }
                if !found
                    && let Some(j) = Self::following(&ordered, i)
                {
                    for d in Self::dates(&ordered[j].text, 0) {
                        spans[j].push(d);
                    }
                }
            }

            if o.entities()
                && let Some(ref ents) = l.entities
            {
                let chars: Vec<char> = t.chars().collect();
                for e in ents {
                    let keep_ent = if e.label == "person" {
                        o.names
                    } else {
                        (e.label == "address" || e.label == "location") && o.addresses
                    };
                    if !keep_ent {
                        continue;
                    }
                    let mut s = (e.start.max(0) as usize).min(char_len);
                    let mut en = ((e.start.saturating_add(e.length)).max(0) as usize)
                        .clamp(s, char_len);
                    while s < en && chars[s].is_whitespace() {
                        s += 1;
                    }
                    while en > s && chars[en - 1].is_whitespace() {
                        en -= 1;
                    }
                    if en > s {
                        spans[i].push((s, en));
                    }
                }
            }

            if o.passwords {
                for &f in secret_fields {
                    for c in 0..char_len {
                        if let Some(cb) = char_box(l, c, char_len)
                            && f.overlap(cb) * 2 >= cb.area()
                            && cb.area() > 0
                        {
                            spans[i].push((c, c + 1));
                        }
                    }
                }
            }
        }

        let mut boxes = Vec::new();
        if o.passwords {
            boxes.extend(secret_fields.iter().copied().filter(|f| f.w > 0 && f.h > 0));
        }
        let mut text = String::new();
        let mut count = 0usize;
        for (i, l) in ordered.iter().enumerate() {
            let char_len = l.text.chars().count();
            let merged = Self::merge(&spans[i], char_len);
            count += merged.len();
            for &(s, e) in &merged {
                boxes.push(Self::span_box(l, s, e));
            }
            let kept = Self::remove(&l.text, &merged);
            if kept.is_empty() {
                continue;
            }
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(&kept);
        }

        let title_char_len = title.chars().count();
        let title_spans = Self::merge(&Self::string_spans(title, o), title_char_len);
        let redacted_title = Self::remove(title, &title_spans);
        RedactionResult {
            boxes,
            text,
            title: redacted_title,
            spans: count + title_spans.len(),
        }
    }

    /// Spans (char indices) found from the string alone: cards, IBAN, SSN, secret tokens, emails, phones.
    pub fn string_spans(t: &str, o: RedactionOptions) -> Vec<(usize, usize)> {
        if t.is_empty() {
            return Vec::new();
        }
        let pat = patterns();
        let c2b = char_byte_map(t);
        let mut out = Vec::new();
        let mut taken: Vec<(usize, usize)> = Vec::new();

        if o.cards {
            for m in pat.card_candidate.find_iter(t).flatten() {
                if let Some((off, len)) = Self::card_span(m.as_str()) {
                    let base = byte_to_char(&c2b, m.start());
                    let sp = (base + off, base + off + len);
                    taken.push(sp);
                    out.push(sp);
                }
            }
        }
        if o.ids {
            for m in pat.iban_candidate.find_iter(t).flatten() {
                if let Some(len) = Self::iban_span(m.as_str()) {
                    let base = byte_to_char(&c2b, m.start());
                    let sp = (base, base + len);
                    taken.push(sp);
                    out.push(sp);
                }
            }
            for m in pat.ssn.find_iter(t).flatten() {
                let sp = (byte_to_char(&c2b, m.start()), byte_to_char(&c2b, m.end()));
                taken.push(sp);
                out.push(sp);
            }
        }
        if o.passwords {
            for m in pat.secret_token.find_iter(t).flatten() {
                out.push((byte_to_char(&c2b, m.start()), byte_to_char(&c2b, m.end())));
            }
        }
        if o.emails {
            for m in pat.email.find_iter(t).flatten() {
                let sp = (byte_to_char(&c2b, m.start()), byte_to_char(&c2b, m.end()));
                taken.push(sp);
                out.push(sp);
            }
        }
        if o.phones {
            let keyword = pat.phone_label.is_match(t).unwrap_or(false);
            for m in pat.phone_candidate.find_iter(t).flatten() {
                let v = m.as_str().trim_end_matches([' ', '.', '-']);
                let d = count_digits(v);
                if !(9..=15).contains(&d) {
                    continue;
                }
                let start_c = byte_to_char(&c2b, m.start());
                let end_c = start_c + v.chars().count();
                if taken.iter().any(|&(a, b)| a < end_c && start_c < b) {
                    continue;
                }
                if pat.date_regexes().iter().any(|r| r.is_match(v).unwrap_or(false)) {
                    continue;
                }
                let shaped = v.starts_with('+')
                    || v.starts_with('(')
                    || v.starts_with('0')
                    || pat.nanp_phone.is_match(v).unwrap_or(false);
                if shaped || keyword {
                    out.push((start_c, end_c));
                }
            }
        }
        out
    }

    /// Spans (char indices) to leave out of an audio transcript, merged (A-M7).
    pub fn transcript_spans(t: &str, o: RedactionOptions) -> Vec<(usize, usize)> {
        let mut spans = Self::string_spans(t, o);
        if t.is_empty() {
            return spans;
        }
        let pat = patterns();
        let c2b = char_byte_map(t);
        let char_len = c2b.len() - 1;
        let chars: Vec<char> = t.chars().collect();

        if o.passwords {
            for m in pat.password_label.find_iter(t).flatten() {
                let s = skip_separators(t, &c2b, byte_to_char(&c2b, m.end()));
                let e = sentence_end(&chars, s);
                if e > s {
                    spans.push((s, e));
                }
            }
        }
        if o.ids {
            for m in pat.id_label.find_iter(t).flatten() {
                let label_end = byte_to_char(&c2b, m.end());
                let s = skip_separators(t, &c2b, label_end);
                if let Ok(Some(v)) = pat.id_value.find_from_pos(t, c2b[s]) {
                    let vs = byte_to_char(&c2b, v.start());
                    let ve = byte_to_char(&c2b, v.end());
                    if vs.saturating_sub(label_end) <= 40 && count_digits(v.as_str()) >= 5 {
                        spans.push((vs, ve));
                    }
                }
            }
        }
        if o.birth_dates
            && let Ok(Some(bm)) = pat.birth_label.find(t)
        {
            spans.extend(Self::dates(t, byte_to_char(&c2b, bm.end())));
        }
        Self::merge(&spans, char_len)
    }

    /// Lines sorted into rows (vertical centres within half a line height) top to bottom, each row left to right.
    pub fn reading_order(lines: &[OcrLine]) -> Vec<OcrLine> {
        let mut by_y: Vec<OcrLine> = lines.to_vec();
        by_y.sort_by(|a, b| {
            let ca = a.y as f64 + a.h as f64 / 2.0;
            let cb = b.y as f64 + b.h as f64 / 2.0;
            ca.total_cmp(&cb)
        });
        let mut rows: Vec<Vec<OcrLine>> = Vec::new();
        for l in by_y {
            let c = l.y as f64 + l.h as f64 / 2.0;
            let fits_last = rows.last().is_some_and(|row| {
                let avg = row.iter().map(|r| r.y as f64 + r.h as f64 / 2.0).sum::<f64>()
                    / row.len() as f64;
                let min_h = row.iter().map(|r| r.h).min().unwrap_or(0) as f64;
                (c - avg).abs() <= (min_h / 2.0).max(4.0)
            });
            if fits_last {
                if let Some(row) = rows.last_mut() {
                    row.push(l);
                }
            } else {
                rows.push(vec![l]);
            }
        }
        let mut out = Vec::with_capacity(lines.len());
        for mut row in rows {
            row.sort_by_key(|l| l.x);
            out.extend(row);
        }
        out
    }

    /// The field that follows a label line (to its right on the same row, else below it).
    pub fn following(lines: &[OcrLine], i: usize) -> Option<usize> {
        let a = &lines[i];
        let mut best: Option<usize> = None;
        let mut best_d = f64::MAX;
        for (j, b) in lines.iter().enumerate() {
            if j == i {
                continue;
            }
            let vo = (a.y + a.h).min(b.y + b.h) - a.y.max(b.y);
            if vo * 2 >= a.h.min(b.h)
                && b.x >= a.x + a.w - 2
                && b.x - (a.x + a.w) <= 12 * a.h
            {
                let d = (b.x - (a.x + a.w)) as f64;
                if d < best_d {
                    best_d = d;
                    best = Some(j);
                }
            }
        }
        if best.is_some() {
            return best;
        }
        for (j, b) in lines.iter().enumerate() {
            if j == i {
                continue;
            }
            let gap = b.y - (a.y + a.h);
            let below = b.y + b.h / 2 > a.y + a.h && gap <= 2 * a.h;
            let aligned = (b.x - a.x).abs() <= 4 * a.h
                || (a.x + a.w).min(b.x + b.w) > a.x.max(b.x);
            if below && aligned {
                let d = gap.max(0) as f64 * 4.0 + (b.x - a.x).abs() as f64;
                if d < best_d {
                    best_d = d;
                    best = Some(j);
                }
            }
        }
        best
    }

    /// Box over characters `[s, e)` of a line, padded; the whole line when cuts are unusable.
    pub fn span_box(l: &OcrLine, s: usize, e: usize) -> PixelBox {
        let mut x0 = l.x;
        let mut x1 = l.x + l.w;
        let char_len = l.text.chars().count();
        if l.cuts.len() == char_len + 1 && e > s && e <= char_len {
            x0 = l.cuts[s];
            x1 = l.cuts[e];
        }
        PixelBox::new(
            x0 - Self::PAD,
            l.y - Self::PAD,
            (x1 - x0).max(1) + 2 * Self::PAD,
            l.h + 2 * Self::PAD,
        )
    }

    pub fn merge(spans: &[(usize, usize)], len: usize) -> Vec<(usize, usize)> {
        let mut sorted: Vec<(usize, usize)> = spans
            .iter()
            .map(|&(s, e)| (s.min(len), e.min(len)))
            .filter(|&(s, e)| e > s)
            .collect();
        sorted.sort_by_key(|&(s, _)| s);
        let mut out: Vec<(usize, usize)> = Vec::new();
        for (s, e) in sorted {
            if let Some(last) = out.last_mut()
                && s <= last.1
            {
                last.1 = last.1.max(e);
            } else {
                out.push((s, e));
            }
        }
        out
    }

    /// Text with char-index spans left out; runs of spaces collapse to one, ends trimmed.
    pub fn remove(text: &str, spans: &[(usize, usize)]) -> String {
        if spans.is_empty() {
            return text.trim().to_string();
        }
        let c2b = char_byte_map(text);
        let mut sb = String::with_capacity(text.len());
        let mut at = 0usize;
        for &(s, e) in spans {
            sb.push_str(&text[c2b[at]..c2b[s]]);
            sb.push(' ');
            at = e;
        }
        sb.push_str(&text[c2b[at]..]);
        patterns()
            .multi_space
            .replace_all(&sb, " ")
            .trim()
            .to_string()
    }

    /// Start and length (char offsets) of a valid card number (13-19 digits, starting with 2-6, Luhn)
    /// in a candidate, else `None`.
    pub fn card_span(candidate: &str) -> Option<(usize, usize)> {
        if is_card(candidate) {
            return Some((0, candidate.chars().count()));
        }
        let chars: Vec<char> = candidate.chars().collect();
        let mut groups: Vec<(usize, usize)> = Vec::new();
        let mut i = 0;
        while i < chars.len() {
            if !chars[i].is_ascii_digit() {
                i += 1;
                continue;
            }
            let s = i;
            while i < chars.len() && chars[i].is_ascii_digit() {
                i += 1;
            }
            groups.push((s, i));
        }
        let c2b = char_byte_map(candidate);
        let mut best: Option<(usize, usize)> = None;
        let mut best_digits = 0usize;
        for a in 0..groups.len() {
            let mut n = 0usize;
            for b in a..groups.len() {
                n += groups[b].1 - groups[b].0;
                if n > 19 {
                    break;
                }
                if n < 13 || n <= best_digits || (a == 0 && b == groups.len() - 1) {
                    continue;
                }
                let sub = &candidate[c2b[groups[a].0]..c2b[groups[b].1]];
                if is_card(sub) {
                    best = Some((groups[a].0, groups[b].1 - groups[a].0));
                    best_digits = n;
                }
            }
        }
        best
    }

    pub fn luhn(digits: &str) -> bool {
        let bytes = digits.as_bytes();
        let mut sum = 0i32;
        let mut dbl = false;
        for &ch in bytes.iter().rev() {
            let mut d = (ch - b'0') as i32;
            if dbl {
                d *= 2;
                if d > 9 {
                    d -= 9;
                }
            }
            sum += d;
            dbl = !dbl;
        }
        sum % 10 == 0
    }

    /// Length (in chars) of the longest prefix of an IBAN candidate that ends at the end or before
    /// a space, has a known IBAN country code, 15..=34 chars, and passes mod 97.
    pub fn iban_span(candidate: &str) -> Option<usize> {
        let bytes = candidate.as_bytes();
        if bytes.len() < 2 {
            return None;
        }
        let cc = std::str::from_utf8(&bytes[..2]).ok()?;
        if !iban_countries().contains(cc) {
            return None;
        }
        for end in (1..=bytes.len()).rev() {
            if bytes[end - 1] == b' ' || (end < bytes.len() && bytes[end] != b' ') {
                continue;
            }
            let compact: String = candidate[..end].chars().filter(|&c| c != ' ').collect();
            if compact.len() < 15 {
                return None;
            }
            if compact.len() > 34 {
                continue;
            }
            if mod97(&compact) == 1 {
                return Some(end);
            }
        }
        None
    }

    /// Date spans (char indices) starting at or after `from_char`.
    pub fn dates(t: &str, from_char: usize) -> Vec<(usize, usize)> {
        let pat = patterns();
        let c2b = char_byte_map(t);
        let mut out = Vec::new();
        for r in pat.date_regexes() {
            for m in r.find_iter(t).flatten() {
                let s = byte_to_char(&c2b, m.start());
                if s >= from_char {
                    out.push((s, byte_to_char(&c2b, m.end())));
                }
            }
        }
        out
    }
}

fn char_byte_map(s: &str) -> Vec<usize> {
    let mut v: Vec<usize> = s.char_indices().map(|(b, _)| b).collect();
    v.push(s.len());
    v
}

fn byte_to_char(c2b: &[usize], byte_idx: usize) -> usize {
    c2b.binary_search(&byte_idx)
        .unwrap_or_else(|pos| pos.min(c2b.len().saturating_sub(1)))
}

fn skip_separators(t: &str, c2b: &[usize], mut i: usize) -> usize {
    let char_len = c2b.len() - 1;
    while i < char_len {
        let ch = t[c2b[i]..].chars().next().unwrap_or('\0');
        if ch.is_whitespace() || matches!(ch, ':' | '=' | '：' | '-' | '–' | '.' | '#') {
            i += 1;
        } else {
            break;
        }
    }
    i
}

fn sentence_end(chars: &[char], i: usize) -> usize {
    for k in i..chars.len() {
        if matches!(chars[k], '.' | '!' | '?')
            && (k + 1 == chars.len() || chars[k + 1].is_whitespace())
        {
            return k + 1;
        }
    }
    chars.len()
}

fn count_digits(s: &str) -> usize {
    s.bytes().filter(u8::is_ascii_digit).count()
}

fn char_box(l: &OcrLine, c: usize, char_len: usize) -> Option<PixelBox> {
    if l.cuts.len() != char_len + 1 || c >= char_len {
        return None;
    }
    Some(PixelBox::new(
        l.cuts[c],
        l.y,
        (l.cuts[c + 1] - l.cuts[c]).max(1),
        l.h,
    ))
}

fn is_card(s: &str) -> bool {
    let digits: String = s.chars().filter(char::is_ascii_digit).collect();
    let b = digits.as_bytes();
    (13..=19).contains(&b.len()) && (b'2'..=b'6').contains(&b[0]) && Redactor::luhn(&digits)
}

fn mod97(iban: &str) -> i32 {
    let mut r = 0i32;
    for ch in iban[4..].chars().chain(iban[..4].chars()) {
        if ch.is_ascii_digit() {
            r = (r * 10 + (ch as i32 - '0' as i32)) % 97;
        } else if ch.is_ascii_uppercase() {
            let v = ch as i32 - 'A' as i32 + 10;
            r = (r * 100 + v) % 97;
        } else {
            return -1;
        }
    }
    r
}

fn iban_countries() -> &'static HashSet<&'static str> {
    static SET: OnceLock<HashSet<&'static str>> = OnceLock::new();
    SET.get_or_init(|| {
        [
            "AD", "AE", "AL", "AT", "AZ", "BA", "BE", "BG", "BH", "BI", "BR", "BY", "CH", "CR",
            "CY", "CZ", "DE", "DJ", "DK", "DO", "EE", "EG", "ES", "FI", "FK", "FO", "FR", "GB",
            "GE", "GI", "GL", "GR", "GT", "HN", "HR", "HU", "IE", "IL", "IQ", "IS", "IT", "JO",
            "KW", "KZ", "LB", "LC", "LI", "LT", "LU", "LV", "LY", "MC", "MD", "ME", "MK", "MN",
            "MR", "MT", "MU", "NI", "NL", "NO", "OM", "PK", "PL", "PS", "PT", "QA", "RO", "RS",
            "RU", "SA", "SC", "SD", "SE", "SI", "SK", "SM", "SO", "ST", "SV", "TL", "TN", "TR",
            "UA", "VA", "VG", "XK", "YE",
        ]
        .into_iter()
        .collect()
    })
}

struct Patterns {
    card_candidate: Regex,
    iban_candidate: Regex,
    ssn: Regex,
    email: Regex,
    phone_candidate: Regex,
    nanp_phone: Regex,
    secret_token: Regex,
    password_label: Regex,
    id_label: Regex,
    id_value: Regex,
    birth_label: Regex,
    phone_label: Regex,
    date_numeric: Regex,
    date_iso: Regex,
    date_day_month: Regex,
    date_month_day: Regex,
    date_cjk: Regex,
    date_vi: Regex,
    multi_space: Regex,
    only_punct: Regex,
}

impl Patterns {
    fn date_regexes(&self) -> [&Regex; 6] {
        [
            &self.date_numeric,
            &self.date_iso,
            &self.date_day_month,
            &self.date_month_day,
            &self.date_cjk,
            &self.date_vi,
        ]
    }
}

fn patterns() -> &'static Patterns {
    static P: OnceLock<Patterns> = OnceLock::new();
    P.get_or_init(|| {
        let r = |s: &str| Regex::new(s).expect("static redactor regex");
        Patterns {
            card_candidate: r(r"(?<!\d)\d(?:[ \-]?\d){12,18}(?!\d)"),
            iban_candidate: r(r"(?<![A-Za-z0-9])[A-Z]{2}\d{2}(?:[ ]?[A-Z0-9]){11,32}"),
            ssn: r(r"(?<!\d)\d{3}-\d{2}-\d{4}(?!\d)"),
            email: r(r"[\p{L}\p{N}._%+\-]+@[\p{L}\p{N}\-]+(?:\.[\p{L}\p{N}\-]+)*\.\p{L}{2,}"),
            phone_candidate: r(r"(?<![\p{L}\p{N}])(?:\+|00)?\(?\d[\d ().\-]{6,}\d(?![\p{L}\p{N}])"),
            nanp_phone: r(r"^\(?\d{3}\)?[ .\-]\d{3}[ .\-]\d{4}$"),
            secret_token: r(r"(?:sk-[A-Za-z0-9_\-]{20,}|gh[pousr]_[A-Za-z0-9]{30,}|github_pat_[A-Za-z0-9_]{30,}|xox[abprs]-[A-Za-z0-9\-]{10,}|AKIA[0-9A-Z]{16}|AIza[0-9A-Za-z_\-]{35}|eyJ[A-Za-z0-9_\-]{10,}\.[A-Za-z0-9_\-]{10,}\.[A-Za-z0-9_\-]{10,})"),
            password_label: r(r"(?i)(?<![\p{L}\p{N}])(?:password|passwort|passwd|pwd|passcode|pass phrase|passphrase|pin code|pin(?=\s*[:=：])|m[ậa]t kh[ẩa]u|kennwort|mot de passe|contrase[ñn]a|senha|wachtwoord|has[łl]o|пароль|parola|l[öo]senord|adgangskode|passord|salasana|heslo|jelsz[óo]|κωδικός|[şs]ifre|api[ _\-]?key|secret key|client secret|access token|auth token|api token|bearer|private key|recovery (?:code|phrase|key)|seed phrase|mnemonic|one-time code|verification code|otp|cvv|cvc|cvv2|security code|m[ãa] otp|m[ãa] x[áa]c (?:nh[ậa]n|th[ựu]c))(?![\p{L}\p{N}])|パスワード|暗証番号|密码|密碼|口令|비밀번호|암호|รหัสผ่าน|كلمة (?:المرور|السر)|סיסמה|पासवर्ड"),
            id_label: r(r"(?i)(?<![\p{L}\p{N}])(?:ssn|social security(?: number| no\.?)?|national (?:id|insurance)(?: number| no\.?)?|id (?:number|no\.?|card)|identity (?:card|number)|passport(?: number| no\.?)?|driver'?s licen[cs]e(?: number)?|driving licen[cs]e|tax id|taxpayer id(?:entification number)?|cccd|cmnd|c[ăa]n c[ưu][ớo]c(?: c[ôo]ng d[âa]n)?|s[ốo] đ[ịi]nh danh|m[ãa] s[ốo] thu[ếe]|personalausweis(?:nummer)?|ausweisnummer|steuer-?id|steuernummer|sozialversicherungsnummer|num[ée]ro de s[ée]curit[ée] sociale|dni|nie|nif|cpf|codice fiscale|pesel|bsn|burgerservicenummer|personnummer|cpr|henkil[öo]tunnus|aadhaar|nric|mykad)(?![\p{L}\p{N}])|身份证(?:号码?)?|身份證(?:號碼?)?|マイナンバー|個人番号|주민등록번호|여권번호|เลขประจำตัวประชาชน"),
            id_value: r(r"(?i)[A-Z0-9](?:[A-Z0-9.\-/]|(?<=\d) (?=\d)){3,}[A-Z0-9]"),
            birth_label: r(r"(?i)(?<![\p{L}\p{N}])(?:date of birth|birth ?date|birthday|born|dob|d\.o\.b\.?|ng[àa]y sinh|sinh ng[àa]y|geburtsdatum|geboren|geb\.|date de naissance|n[ée]e? le|fecha de nacimiento|data de nascimento|data di nascita|дата рождения|data urodzenia|geboortedatum|f[öo]delsedatum|f[øo]dselsdato|syntym[äa]aika|datum narozen[íi]|sz[üu]let[ée]si (?:id[őo]|d[áa]tum)|do[ğg]um tarihi)(?![\p{L}\p{N}])|生年月日|出生日期|出生年月|生日|생년월일|วันเกิด|تاريخ الميلاد|תאריך לידה|जन्म तिथि"),
            phone_label: r(r"(?i)(?<![\p{L}\p{N}])(?:phone|tel|telephone|mobile|cell|fax|đi[ệe]n tho[ạa]i|s[đd]t|đt|telefon|t[ée]l[ée]phone|tel[ée]fono|telefone|телефон|電話|电话|手机|전화|휴대폰)(?![\p{L}\p{N}])"),
            date_numeric: r(r"(?<!\d)\d{1,2}[./\-]\d{1,2}[./\-](?:\d{4}|\d{2})(?!\d)"),
            date_iso: r(r"(?<!\d)\d{4}[./\-]\d{1,2}[./\-]\d{1,2}(?!\d)"),
            date_day_month: r(r"(?<!\d)\d{1,2}\.?\s+\p{L}{3,12}\.?,?\s+\d{4}(?!\d)"),
            date_month_day: r(r"(?<![\p{L}])\p{L}{3,12}\.?\s+\d{1,2}(?:st|nd|rd|th)?,?\s+\d{4}(?!\d)"),
            date_cjk: r(r"\d{4}\s*[年년]\s*\d{1,2}\s*[月월]\s*\d{1,2}\s*[日일]"),
            date_vi: r(r"(?i)(?:ng[àa]y\s+)?\d{1,2}\s+th[áa]ng\s+\d{1,2},?\s+(?:n[ăa]m\s+)?\d{4}"),
            multi_space: r(r" {2,}"),
            only_punct: r(r"^\p{P}*$"),
        }
    })
}
