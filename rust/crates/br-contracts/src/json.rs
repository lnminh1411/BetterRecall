// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

use std::io;

use serde::Serialize;
use serde_json::ser::{CharEscape, Formatter};
use serde_json::{Map, Serializer, Value};

/// Serializes finite integer-valued floats without `.0` (matching `System.Text.Json` / `ContractsJson`).
pub mod dotnet_float {
    use serde::Serializer;

    const MAX_EXACT_F64_INT: f64 = 9_007_199_254_740_991.0;
    const MAX_EXACT_F32_INT: f32 = 16_777_215.0;

    pub fn ser_f64<S: Serializer>(v: &f64, s: S) -> Result<S::Ok, S::Error> {
        let x = *v;
        if x.is_finite()
            && x.fract() == 0.0
            && x.abs() <= MAX_EXACT_F64_INT
            && !(x == 0.0 && x.is_sign_negative())
        {
            s.serialize_i64(x as i64)
        } else {
            s.serialize_f64(x)
        }
    }

    pub fn ser_opt_f64<S: Serializer>(v: &Option<f64>, s: S) -> Result<S::Ok, S::Error> {
        match v {
            Some(x) => ser_f64(x, s),
            None => s.serialize_none(),
        }
    }

    pub fn ser_f32<S: Serializer>(v: &f32, s: S) -> Result<S::Ok, S::Error> {
        let x = *v;
        if x.is_finite()
            && x.fract() == 0.0
            && x.abs() <= MAX_EXACT_F32_INT
            && !(x == 0.0 && x.is_sign_negative())
        {
            s.serialize_i64(x as i64)
        } else {
            s.serialize_f32(x)
        }
    }

    pub fn ser_opt_f32<S: Serializer>(v: &Option<f32>, s: S) -> Result<S::Ok, S::Error> {
        match v {
            Some(x) => ser_f32(x, s),
            None => s.serialize_none(),
        }
    }
}

/// Standard base64 serialization for `byte[]` fields (matching `System.Text.Json`).
pub mod dotnet_base64 {
    use serde::{Deserialize, Deserializer, Serializer};

    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    pub fn encode(input: &[u8]) -> String {
        let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
        for chunk in input.chunks(3) {
            let b0 = chunk[0] as u32;
            let b1 = *chunk.get(1).unwrap_or(&0) as u32;
            let b2 = *chunk.get(2).unwrap_or(&0) as u32;
            let triple = (b0 << 16) | (b1 << 8) | b2;
            out.push(TABLE[((triple >> 18) & 0x3F) as usize] as char);
            out.push(TABLE[((triple >> 12) & 0x3F) as usize] as char);
            if chunk.len() > 1 {
                out.push(TABLE[((triple >> 6) & 0x3F) as usize] as char);
            } else {
                out.push('=');
            }
            if chunk.len() > 2 {
                out.push(TABLE[(triple & 0x3F) as usize] as char);
            } else {
                out.push('=');
            }
        }
        out
    }

    pub fn decode(input: &str) -> Result<Vec<u8>, &'static str> {
        let bytes = input.as_bytes();
        if bytes.is_empty() {
            return Ok(Vec::new());
        }
        if !bytes.len().is_multiple_of(4) {
            return Err("invalid base64 length");
        }
        let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
        let val = |b: u8| -> Result<u32, &'static str> {
            match b {
                b'A'..=b'Z' => Ok((b - b'A') as u32),
                b'a'..=b'z' => Ok((b - b'a' + 26) as u32),
                b'0'..=b'9' => Ok((b - b'0' + 52) as u32),
                b'+' => Ok(62),
                b'/' => Ok(63),
                _ => Err("invalid base64 character"),
            }
        };
        for (idx, chunk) in bytes.chunks_exact(4).enumerate() {
            let is_last = (idx + 1) * 4 == bytes.len();
            let v0 = val(chunk[0])?;
            let v1 = val(chunk[1])?;
            let pad2 = chunk[2] == b'=';
            let pad3 = chunk[3] == b'=';
            if (pad2 && !pad3) || (pad3 && !is_last) {
                return Err("invalid base64 padding");
            }
            let v2 = if pad2 { 0 } else { val(chunk[2])? };
            let v3 = if pad3 { 0 } else { val(chunk[3])? };
            let triple = (v0 << 18) | (v1 << 12) | (v2 << 6) | v3;
            out.push((triple >> 16) as u8);
            if !pad2 {
                out.push(((triple >> 8) & 0xFF) as u8);
            }
            if !pad3 {
                out.push((triple & 0xFF) as u8);
            }
        }
        Ok(out)
    }

    pub fn serialize<S: Serializer>(bytes: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&encode(bytes))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let s = String::deserialize(d)?;
        decode(&s).map_err(serde::de::Error::custom)
    }

    pub mod opt {
        use super::*;

        pub fn serialize<S: Serializer>(bytes: &Option<Vec<u8>>, s: S) -> Result<S::Ok, S::Error> {
            match bytes {
                Some(b) => s.serialize_str(&encode(b)),
                None => s.serialize_none(),
            }
        }

        pub fn deserialize<'de, D: Deserializer<'de>>(
            d: D,
        ) -> Result<Option<Vec<u8>>, D::Error> {
            let opt = Option::<String>::deserialize(d)?;
            opt.map(|s| decode(&s).map_err(serde::de::Error::custom))
                .transpose()
        }
    }
}

/// ISO 8601 UTC timestamp serialization matching `UtcTimestampConverter` (`yyyy-MM-dd'T'HH:mm:ss.FFF'Z'`).
pub mod utc_timestamp {
    use chrono::{DateTime, FixedOffset, NaiveDate, NaiveDateTime, Utc};
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn format_utc(dt: &DateTime<Utc>) -> String {
        let base = dt.format("%Y-%m-%dT%H:%M:%S").to_string();
        let ms = dt.timestamp_subsec_millis().min(999);
        if ms == 0 {
            format!("{base}Z")
        } else {
            let frac = format!("{ms:03}");
            let trimmed = frac.trim_end_matches('0');
            format!("{base}.{trimmed}Z")
        }
    }

    pub fn parse_utc(s: &str) -> Result<DateTime<Utc>, &'static str> {
        let t = s.trim();
        if let Ok(dt) = DateTime::<FixedOffset>::parse_from_rfc3339(t) {
            return Ok(dt.with_timezone(&Utc));
        }
        for fmt in [
            "%Y-%m-%dT%H:%M:%S%.f",
            "%Y-%m-%dT%H:%M:%S",
            "%Y-%m-%d %H:%M:%S%.f",
            "%Y-%m-%d %H:%M:%S",
        ] {
            if let Ok(ndt) = NaiveDateTime::parse_from_str(t, fmt) {
                return Ok(ndt.and_utc());
            }
        }
        if let Ok(nd) = NaiveDate::parse_from_str(t, "%Y-%m-%d")
            && let Some(ndt) = nd.and_hms_opt(0, 0, 0)
        {
            return Ok(ndt.and_utc());
        }
        Err("Bad timestamp.")
    }

    pub fn serialize<S: Serializer>(dt: &DateTime<Utc>, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&format_utc(dt))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<DateTime<Utc>, D::Error> {
        let s = Option::<String>::deserialize(d)?
            .ok_or_else(|| serde::de::Error::custom("Bad timestamp."))?;
        parse_utc(&s).map_err(serde::de::Error::custom)
    }

    pub mod opt {
        use super::*;

        pub fn serialize<S: Serializer>(
            dt: &Option<DateTime<Utc>>,
            s: S,
        ) -> Result<S::Ok, S::Error> {
            match dt {
                Some(v) => super::serialize(v, s),
                None => s.serialize_none(),
            }
        }

        pub fn deserialize<'de, D: Deserializer<'de>>(
            d: D,
        ) -> Result<Option<DateTime<Utc>>, D::Error> {
            let opt = Option::<String>::deserialize(d)?;
            opt.map(|s| parse_utc(&s).map_err(serde::de::Error::custom))
                .transpose()
        }
    }
}

/// Formatter matching .NET `System.Text.Json` with `JavaScriptEncoder.Default` (compact).
pub struct DotNetFormatter;

/// Pretty formatter matching .NET `System.Text.Json` with `WriteIndented = true` (2-space indent, `\r\n`, `JavaScriptEncoder.Default`).
pub struct DotNetPrettyFormatter {
    indent: usize,
    has_value: bool,
}

impl DotNetPrettyFormatter {
    pub fn new() -> Self {
        Self {
            indent: 0,
            has_value: false,
        }
    }
}

impl Default for DotNetPrettyFormatter {
    fn default() -> Self {
        Self::new()
    }
}

fn write_dotnet_char_escape<W: io::Write + ?Sized>(
    writer: &mut W,
    char_escape: CharEscape,
) -> io::Result<()> {
    let s = match char_escape {
        CharEscape::Quote => b"\\u0022".as_slice(),
        CharEscape::ReverseSolidus => b"\\\\".as_slice(),
        CharEscape::Solidus => b"/".as_slice(),
        CharEscape::Backspace => b"\\b".as_slice(),
        CharEscape::FormFeed => b"\\f".as_slice(),
        CharEscape::LineFeed => b"\\n".as_slice(),
        CharEscape::CarriageReturn => b"\\r".as_slice(),
        CharEscape::Tab => b"\\t".as_slice(),
        CharEscape::AsciiControl(b) => {
            return write!(writer, "\\u{b:04X}");
        }
    };
    writer.write_all(s)
}

fn write_dotnet_fragment<W: io::Write + ?Sized>(
    writer: &mut W,
    fragment: &str,
) -> io::Result<()> {
    let mut last = 0;
    for (idx, ch) in fragment.char_indices() {
        let esc = match ch {
            '&' => Some("\\u0026"),
            '\'' => Some("\\u0027"),
            '+' => Some("\\u002B"),
            '<' => Some("\\u003C"),
            '>' => Some("\\u003E"),
            '\x7F' => Some("\\u007F"),
            c if !c.is_ascii() => None,
            _ => continue,
        };
        if idx > last {
            writer.write_all(&fragment.as_bytes()[last..idx])?;
        }
        if let Some(lit) = esc {
            writer.write_all(lit.as_bytes())?;
        } else {
            let mut utf16 = [0u16; 2];
            for code in ch.encode_utf16(&mut utf16) {
                write!(writer, "\\u{code:04X}")?;
            }
        }
        last = idx + ch.len_utf8();
    }
    if last < fragment.len() {
        writer.write_all(&fragment.as_bytes()[last..])?;
    }
    Ok(())
}

impl Formatter for DotNetFormatter {
    #[inline]
    fn write_string_fragment<W>(&mut self, writer: &mut W, fragment: &str) -> io::Result<()>
    where
        W: io::Write + ?Sized,
    {
        write_dotnet_fragment(writer, fragment)
    }

    #[inline]
    fn write_char_escape<W>(&mut self, writer: &mut W, char_escape: CharEscape) -> io::Result<()>
    where
        W: io::Write + ?Sized,
    {
        write_dotnet_char_escape(writer, char_escape)
    }
}

fn write_indent<W: io::Write + ?Sized>(writer: &mut W, level: usize) -> io::Result<()> {
    for _ in 0..level {
        writer.write_all(b"  ")?;
    }
    Ok(())
}

impl Formatter for DotNetPrettyFormatter {
    #[inline]
    fn write_string_fragment<W>(&mut self, writer: &mut W, fragment: &str) -> io::Result<()>
    where
        W: io::Write + ?Sized,
    {
        write_dotnet_fragment(writer, fragment)
    }

    #[inline]
    fn write_char_escape<W>(&mut self, writer: &mut W, char_escape: CharEscape) -> io::Result<()>
    where
        W: io::Write + ?Sized,
    {
        write_dotnet_char_escape(writer, char_escape)
    }

    #[inline]
    fn begin_array<W>(&mut self, writer: &mut W) -> io::Result<()>
    where
        W: io::Write + ?Sized,
    {
        self.indent += 1;
        self.has_value = false;
        writer.write_all(b"[")
    }

    #[inline]
    fn end_array<W>(&mut self, writer: &mut W) -> io::Result<()>
    where
        W: io::Write + ?Sized,
    {
        self.indent -= 1;
        if self.has_value {
            writer.write_all(b"\r\n")?;
            write_indent(writer, self.indent)?;
        }
        writer.write_all(b"]")
    }

    #[inline]
    fn begin_array_value<W>(&mut self, writer: &mut W, first: bool) -> io::Result<()>
    where
        W: io::Write + ?Sized,
    {
        writer.write_all(if first { b"\r\n" } else { b",\r\n" })?;
        write_indent(writer, self.indent)
    }

    #[inline]
    fn end_array_value<W>(&mut self, _writer: &mut W) -> io::Result<()>
    where
        W: io::Write + ?Sized,
    {
        self.has_value = true;
        Ok(())
    }

    #[inline]
    fn begin_object<W>(&mut self, writer: &mut W) -> io::Result<()>
    where
        W: io::Write + ?Sized,
    {
        self.indent += 1;
        self.has_value = false;
        writer.write_all(b"{")
    }

    #[inline]
    fn end_object<W>(&mut self, writer: &mut W) -> io::Result<()>
    where
        W: io::Write + ?Sized,
    {
        self.indent -= 1;
        if self.has_value {
            writer.write_all(b"\r\n")?;
            write_indent(writer, self.indent)?;
        }
        writer.write_all(b"}")
    }

    #[inline]
    fn begin_object_key<W>(&mut self, writer: &mut W, first: bool) -> io::Result<()>
    where
        W: io::Write + ?Sized,
    {
        writer.write_all(if first { b"\r\n" } else { b",\r\n" })?;
        write_indent(writer, self.indent)
    }

    #[inline]
    fn begin_object_value<W>(&mut self, writer: &mut W) -> io::Result<()>
    where
        W: io::Write + ?Sized,
    {
        writer.write_all(b": ")
    }

    #[inline]
    fn end_object_value<W>(&mut self, _writer: &mut W) -> io::Result<()>
    where
        W: io::Write + ?Sized,
    {
        self.has_value = true;
        Ok(())
    }
}

/// Serializes `value` identically to .NET `ContractsJson` / `CoreJson` (compact, camelCase, nulls omitted, `JavaScriptEncoder.Default`).
pub fn to_json_string<T: Serialize>(value: &T) -> serde_json::Result<String> {
    let mut buf = Vec::with_capacity(128);
    let mut ser = Serializer::with_formatter(&mut buf, DotNetFormatter);
    value.serialize(&mut ser)?;
    Ok(String::from_utf8(buf).expect("serde_json emits valid UTF-8"))
}

/// Serializes `value` identically to .NET `PrettySettingsJson` (indented 2 spaces, `\r\n`, `JavaScriptEncoder.Default`).
pub fn to_pretty_json_string<T: Serialize>(value: &T) -> serde_json::Result<String> {
    let mut buf = Vec::with_capacity(512);
    let mut ser = Serializer::with_formatter(&mut buf, DotNetPrettyFormatter::new());
    value.serialize(&mut ser)?;
    Ok(String::from_utf8(buf).expect("serde_json emits valid UTF-8"))
}

/// Strips UTF-8 BOM, `//` line comments, `/* ... */` block comments, and trailing commas outside strings.
pub fn strip_jsonc(input: &str) -> String {
    let s = input.strip_prefix('\u{FEFF}').unwrap_or(input);
    let no_comments = strip_comments(s);
    strip_trailing_commas(&no_comments)
}

/// Parses JSONC text into a root JSON object map, failing on invalid JSON or non-object roots.
pub fn parse_jsonc_object(input: &str) -> Result<Map<String, Value>, String> {
    let cleaned = strip_jsonc(input);
    let value: Value = serde_json::from_str(&cleaned).map_err(|e| format!("JsonException: {e}"))?;
    match value {
        Value::Object(obj) => Ok(obj),
        _ => Err("JsonException: Not an object.".to_string()),
    }
}

/// Merges `incoming` keys one by one into `target`, calling `on_bad_key(section_name, key)` for keys whose type fails deserialization.
pub fn merge_section<T>(
    section_name: &str,
    incoming: Map<String, Value>,
    target: &mut T,
    mut on_bad_key: impl FnMut(&str, &str),
) where
    T: Serialize + serde::de::DeserializeOwned,
{
    let Ok(Value::Object(mut merged)) = serde_json::to_value(&*target) else {
        return;
    };
    for (key, value) in incoming {
        let mut candidate = merged.clone();
        candidate.insert(key.clone(), value);
        if let Ok(parsed) = serde_json::from_value::<T>(Value::Object(candidate.clone())) {
            *target = parsed;
            merged = candidate;
        } else {
            on_bad_key(section_name, &key);
        }
    }
}

fn strip_comments(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    let mut in_string = false;
    let mut escaped = false;
    while i < bytes.len() {
        if in_string {
            let ch = s[i..].chars().next().unwrap();
            out.push(ch);
            i += ch.len_utf8();
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }
        if bytes[i] == b'"' {
            in_string = true;
            out.push('"');
            i += 1;
            continue;
        }
        if bytes[i] == b'/' && i + 1 < bytes.len() {
            if bytes[i + 1] == b'/' {
                i += 2;
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
                continue;
            }
            if bytes[i + 1] == b'*' {
                i += 2;
                let mut closed = false;
                while i + 1 < bytes.len() {
                    if bytes[i] == b'*' && bytes[i + 1] == b'/' {
                        i += 2;
                        closed = true;
                        break;
                    }
                    if bytes[i] == b'\n' {
                        out.push('\n');
                    }
                    i += 1;
                }
                if !closed {
                    // Leave invalid unclosed block comment token so serde_json fails cleanly.
                    out.push_str("/*");
                    break;
                }
                out.push(' ');
                continue;
            }
        }
        let ch = s[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

fn strip_trailing_commas(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    let mut in_string = false;
    let mut escaped = false;
    while i < bytes.len() {
        if in_string {
            let ch = s[i..].chars().next().unwrap();
            out.push(ch);
            i += ch.len_utf8();
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }
        if bytes[i] == b'"' {
            in_string = true;
            out.push('"');
            i += 1;
            continue;
        }
        if bytes[i] == b',' {
            let mut j = i + 1;
            while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                j += 1;
            }
            if j < bytes.len() && (bytes[j] == b'}' || bytes[j] == b']') {
                i += 1;
                continue;
            }
        }
        let ch = s[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}
