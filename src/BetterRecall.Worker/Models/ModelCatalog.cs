// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
namespace BetterRecall.Worker.Models;

/// <summary>Folder names under inference.modelDir and the names used in hello.missing and in
/// "Model not installed: &lt;name&gt;." errors.</summary>
public static class ModelCatalog
{
    public const string EmbeddingGemma = "embeddinggemma-2";
    public const string Siglip = "siglip2-base";
    public const string Whisper = "whisper-large-v3-turbo";
    public const string Vad = "silero-vad";
    public const string Nsfw = "nsfw-classifier";
    public const string NsfwDetector = "nsfw-detector";
    public const string Ocr = "ocr";
    public const string Ner = "ner";
    /// <summary>Name in "Model not installed" when ocr.engine is windows and no Windows OCR language is installed.</summary>
    public const string WindowsOcr = "windows-ocr";

    public const string VadFile = "silero_vad.onnx";

    public static string Dir(string modelDir, string name) => Path.Combine(modelDir, name);

    /// <summary>Manifest when present and every listed file exists, else null.</summary>
    public static ModelManifest? Load(string modelDir, string name)
    {
        try
        {
            var m = ModelManifest.TryLoad(Dir(modelDir, name));
            if (m is null) return null;
            if (!m.FilesPresent(out var missing))
            {
                Hosting.Log.Warn($"model {name}: file {missing} missing or wrong size");
                return null;
            }
            return m;
        }
        catch (Exception e) when (e is IOException or UnauthorizedAccessException or System.Text.Json.JsonException or InvalidDataException)
        {
            Hosting.Log.Warn($"model {name}: manifest unreadable ({e.GetType().Name})");
            return null;
        }
    }

    /// <summary>OpenVINO GenAI Whisper folder: encoder, decoder, tokenizer and detokenizer IR.</summary>
    public static bool WhisperPresent(string modelDir)
    {
        var d = Dir(modelDir, Whisper);
        return File.Exists(Path.Combine(d, "openvino_encoder_model.xml"))
            && File.Exists(Path.Combine(d, "openvino_decoder_model.xml"))
            && File.Exists(Path.Combine(d, "openvino_tokenizer.xml"))
            && File.Exists(Path.Combine(d, "openvino_detokenizer.xml"));
    }

    public static string? VadPath(string modelDir)
    {
        var p = Path.Combine(Dir(modelDir, Vad), VadFile);
        return File.Exists(p) ? p : null;
    }
}
