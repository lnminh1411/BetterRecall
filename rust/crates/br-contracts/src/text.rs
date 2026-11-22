// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Small string helpers that are safe on any UTF-8 input.

/// `s` without `suffix` when `s` ends with it (ASCII case-insensitive), else `None`.
/// Never slices inside a multi-byte character, so names like `日日` cannot panic (X1/N1).
pub fn strip_suffix_ci<'a>(s: &'a str, suffix: &str) -> Option<&'a str> {
    let cut = s.len().checked_sub(suffix.len())?;
    if s.is_char_boundary(cut) && s[cut..].eq_ignore_ascii_case(suffix) {
        Some(&s[..cut])
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::strip_suffix_ci;

    #[test]
    fn strips_ascii_suffix_and_never_panics_on_multibyte() {
        assert_eq!(Some("chrome"), strip_suffix_ci("chrome.EXE", ".exe"));
        assert_eq!(Some(""), strip_suffix_ci(".exe", ".exe"));
        assert_eq!(None, strip_suffix_ci("exe", ".exe"));
        assert_eq!(None, strip_suffix_ci("日日", ".exe"));
        assert_eq!(None, strip_suffix_ci("abcd-€db", ".db"));
        assert_eq!(Some("日本語"), strip_suffix_ci("日本語.exe", ".exe"));
        assert_eq!(None, strip_suffix_ci("é", ".exe"));
    }
}
