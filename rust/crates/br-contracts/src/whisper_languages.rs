// SPDX-License-Identifier: MIT
// Copyright (c) BetterRecall contributors

use std::collections::HashSet;
use std::sync::LazyLock;

/// `audio.language` values Whisper has a language token for (`<|xx|>`, the `lang_to_id` table of
/// the multilingual models, large-v3 included).
pub struct WhisperLanguages;

impl WhisperLanguages {
    pub const ALL_CODES: [&'static str; 100] = [
        "en", "zh", "de", "es", "ru", "ko", "fr", "ja", "pt", "tr", "pl", "ca", "nl", "ar", "sv",
        "it", "id", "hi", "fi", "vi", "he", "uk", "el", "ms", "cs", "ro", "da", "hu", "ta", "no",
        "th", "ur", "hr", "bg", "lt", "la", "mi", "ml", "cy", "sk", "te", "fa", "lv", "bn", "sr",
        "az", "sl", "kn", "et", "mk", "br", "eu", "is", "hy", "ne", "mn", "bs", "kk", "sq", "sw",
        "gl", "mr", "pa", "si", "km", "sn", "yo", "so", "af", "oc", "ka", "be", "tg", "sd", "gu",
        "am", "yi", "lo", "uz", "fo", "ht", "ps", "tk", "nn", "mt", "sa", "lb", "my", "bo", "tl",
        "mg", "as", "tt", "haw", "ln", "ha", "ba", "jw", "su", "yue",
    ];

    /// `"auto"` or a code in `ALL_CODES` (lower case, as stored).
    pub fn is_valid(value: &str) -> bool {
        static SET: LazyLock<HashSet<&'static str>> =
            LazyLock::new(|| WhisperLanguages::ALL_CODES.into_iter().collect());
        value == "auto" || SET.contains(value)
    }
}
