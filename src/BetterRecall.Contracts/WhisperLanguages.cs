// SPDX-License-Identifier: MIT
// Copyright (c) BetterRecall contributors
namespace BetterRecall.Contracts;

/// <summary>audio.language values Whisper has a language token for (<c>&lt;|xx|&gt;</c>, the lang_to_id table of
/// the multilingual models, large-v3 included). Core settings validation and the worker share this list, so an unknown
/// code is refused up front instead of failing every transcription.</summary>
public static class WhisperLanguages
{
    public static readonly IReadOnlySet<string> Codes = new HashSet<string>(StringComparer.Ordinal)
    {
        "en", "zh", "de", "es", "ru", "ko", "fr", "ja", "pt", "tr", "pl", "ca", "nl", "ar", "sv", "it", "id", "hi", "fi", "vi",
        "he", "uk", "el", "ms", "cs", "ro", "da", "hu", "ta", "no", "th", "ur", "hr", "bg", "lt", "la", "mi", "ml", "cy", "sk",
        "te", "fa", "lv", "bn", "sr", "az", "sl", "kn", "et", "mk", "br", "eu", "is", "hy", "ne", "mn", "bs", "kk", "sq", "sw",
        "gl", "mr", "pa", "si", "km", "sn", "yo", "so", "af", "oc", "ka", "be", "tg", "sd", "gu", "am", "yi", "lo", "uz", "fo",
        "ht", "ps", "tk", "nn", "mt", "sa", "lb", "my", "bo", "tl", "mg", "as", "tt", "haw", "ln", "ha", "ba", "jw", "su", "yue",
    };

    /// <summary>"auto" or a code in <see cref="Codes"/> (lower case, as stored).</summary>
    public static bool IsValid(string? value) => value == "auto" || (value is not null && Codes.Contains(value));
}
