// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Runtime.InteropServices;
using System.Text;
using BetterRecall.Contracts;
using BetterRecall.Worker.Hosting;
using BetterRecall.Worker.Protocol;

namespace BetterRecall.Worker.Audio;

/// <summary>Whisper through OpenVINO GenAI's WhisperPipeline C API (openvino_genai_c.dll, 2026.4). GenAI owns the
/// encoder/decoder loop, KV cache and the static reshaping the NPU needs, which no .NET binding offers.
/// Signatures from src/c/include/openvino/genai/c/whisper_pipeline.h and whisper_generation_config.h (2026.4).</summary>
public sealed unsafe partial class WhisperGenAi : IDisposable
{
    private const string Lib = "openvino_genai_c";
    private nint _pipe;

    public string DeviceId { get; }

    [StructLayout(LayoutKind.Sequential)]
    private struct ov_property_t { public nint key; public nint value; }

    // Variadic create: on Windows x64 a variadic callee reads extra args from the same slots as fixed ones, so a
    // fixed-arity import with (key, value) pairs is ABI-identical. One import per pair count used here.
    [LibraryImport(Lib, StringMarshalling = StringMarshalling.Utf8, EntryPoint = "ov_genai_whisper_pipeline_create")]
    private static partial int Create0(string modelsPath, string device, nuint n, out nint pipeline);
    [LibraryImport(Lib, StringMarshalling = StringMarshalling.Utf8, EntryPoint = "ov_genai_whisper_pipeline_create")]
    private static partial int Create2(string modelsPath, string device, nuint n, out nint pipeline, string k1, string v1);
    [LibraryImport(Lib, StringMarshalling = StringMarshalling.Utf8, EntryPoint = "ov_genai_whisper_pipeline_create")]
    private static partial int Create4(string modelsPath, string device, nuint n, out nint pipeline, string k1, string v1, string k2, string v2);

    [LibraryImport(Lib)] private static partial void ov_genai_whisper_pipeline_free(nint pipeline);
    [LibraryImport(Lib)] private static partial int ov_genai_whisper_pipeline_get_generation_config(nint pipeline, out nint config);
    [LibraryImport(Lib)] private static partial void ov_genai_whisper_generation_config_free(nint config);
    [LibraryImport(Lib, StringMarshalling = StringMarshalling.Utf8)]
    private static partial int ov_genai_whisper_generation_config_set_language(nint config, string? language);
    [LibraryImport(Lib, StringMarshalling = StringMarshalling.Utf8)]
    private static partial int ov_genai_whisper_generation_config_set_task(nint config, string task);
    [LibraryImport(Lib)]
    private static partial int ov_genai_whisper_generation_config_set_return_timestamps(nint config, [MarshalAs(UnmanagedType.U1)] bool value);
    [LibraryImport(Lib)]
    private static partial int ov_genai_whisper_pipeline_generate(nint pipeline, float* speech, nuint size, nint config, out nint results);
    [LibraryImport(Lib)] private static partial void ov_genai_whisper_decoded_results_free(nint results);
    [LibraryImport(Lib)] private static partial int ov_genai_whisper_decoded_results_get_string(nint results, byte* output, nuint* size);
    [LibraryImport(Lib)] private static partial int ov_genai_whisper_decoded_results_has_chunks(nint results, [MarshalAs(UnmanagedType.U1)] out bool has);
    [LibraryImport(Lib)] private static partial int ov_genai_whisper_decoded_results_get_chunks_count(nint results, out nuint count);
    [LibraryImport(Lib)] private static partial int ov_genai_whisper_decoded_results_get_chunk_at(nint results, nuint index, out nint chunk);
    [LibraryImport(Lib)] private static partial void ov_genai_whisper_decoded_result_chunk_free(nint chunk);
    [LibraryImport(Lib)] private static partial int ov_genai_whisper_decoded_result_chunk_get_start_ts(nint chunk, out float ts);
    [LibraryImport(Lib)] private static partial int ov_genai_whisper_decoded_result_chunk_get_end_ts(nint chunk, out float ts);
    [LibraryImport(Lib)] private static partial int ov_genai_whisper_decoded_result_chunk_get_text(nint chunk, byte* text, nuint* size);

    /// <summary>True when openvino_genai_c.dll loads.</summary>
    public static bool RuntimePresent()
    {
        try { return NativeLibrary.TryLoad(Lib, typeof(WhisperGenAi).Assembly, null, out _); }
        catch { return false; }
    }

    public WhisperGenAi(string modelDir, string deviceId, string? cacheDir)
    {
        DeviceId = deviceId;
        int st;
        try
        {
            st = cacheDir is null
                ? Create0(modelDir, deviceId, 0, out _pipe)
                : deviceId.StartsWith("NPU", StringComparison.Ordinal)
                    ? Create4(modelDir, deviceId, 4, out _pipe, "CACHE_DIR", cacheDir, "NPU_TURBO", "NO")
                    : Create2(modelDir, deviceId, 2, out _pipe, "CACHE_DIR", cacheDir);
        }
        catch (DllNotFoundException) { throw new DeviceFailedException("OpenVINO GenAI runtime not found"); }
        catch (EntryPointNotFoundException) { throw new DeviceFailedException("OpenVINO GenAI runtime too old"); }
        if (st != 0 || _pipe == 0) throw new DeviceFailedException($"Whisper on {deviceId} failed to load (status {st})");
    }

    /// <summary>language "auto" lets Whisper detect it. The C API does not expose the detected language, so the
    /// result reports "und" in that case.</summary>
    public TranscribeResult Transcribe(float[] pcm, string language, long started)
    {
        ObjectDisposedException.ThrowIf(_pipe == 0, this);
        Check(ov_genai_whisper_pipeline_get_generation_config(_pipe, out var cfg), "config");
        nint res = 0;
        try
        {
            Check(ov_genai_whisper_generation_config_set_task(cfg, "transcribe"), "task");
            Check(ov_genai_whisper_generation_config_set_language(cfg, language == "auto" ? null : $"<|{language}|>"), "language");
            Check(ov_genai_whisper_generation_config_set_return_timestamps(cfg, true), "timestamps");
            fixed (float* p = pcm) Check(ov_genai_whisper_pipeline_generate(_pipe, p, (nuint)pcm.Length, cfg, out res), "generate");

            string text = ReadString((b, s) => ov_genai_whisper_decoded_results_get_string(res, b, s)).Trim();
            var segs = new List<TranscriptSegment>();
            if (ov_genai_whisper_decoded_results_has_chunks(res, out var has) == 0 && has
                && ov_genai_whisper_decoded_results_get_chunks_count(res, out var count) == 0)
            {
                for (nuint i = 0; i < count; i++)
                {
                    if (ov_genai_whisper_decoded_results_get_chunk_at(res, i, out var ch) != 0) continue;
                    try
                    {
                        ov_genai_whisper_decoded_result_chunk_get_start_ts(ch, out var s0);
                        ov_genai_whisper_decoded_result_chunk_get_end_ts(ch, out var s1);
                        var t = ReadString((b, s) => ov_genai_whisper_decoded_result_chunk_get_text(ch, b, s)).Trim();
                        int endMs = s1 >= s0 ? (int)(s1 * 1000) : pcm.Length * 1000 / 16000;
                        if (t.Length > 0) segs.Add(new TranscriptSegment((int)(s0 * 1000), endMs, t));
                    }
                    finally { ov_genai_whisper_decoded_result_chunk_free(ch); }
                }
            }
            if (segs.Count == 0 && text.Length > 0) segs.Add(new TranscriptSegment(0, pcm.Length * 1000 / 16000, text));
            long ms = (System.Diagnostics.Stopwatch.GetTimestamp() - started) * 1000 / System.Diagnostics.Stopwatch.Frequency;
            return new TranscribeResult(text.Length > 0, text, language == "auto" ? "und" : language, segs.ToArray(), ms);
        }
        finally
        {
            if (res != 0) ov_genai_whisper_decoded_results_free(res);
            ov_genai_whisper_generation_config_free(cfg);
        }
    }

    private delegate int StringGetter(byte* buf, nuint* size);

    /// <summary>GenAI string getters: call with null to learn the size (terminator included), then fill.</summary>
    private static string ReadString(StringGetter get)
    {
        nuint size = 0;
        if (get(null, &size) != 0 || size == 0) return "";
        var buf = new byte[(int)size];
        fixed (byte* b = buf) { if (get(b, &size) != 0) return ""; }
        int n = Array.IndexOf(buf, (byte)0);
        return Encoding.UTF8.GetString(buf, 0, n < 0 ? buf.Length : n);
    }

    private static void Check(int st, string what)
    {
        if (st != 0) throw new DeviceFailedException($"Whisper {what} failed (status {st})");
    }

    public void Dispose()
    {
        if (_pipe != 0) { ov_genai_whisper_pipeline_free(_pipe); _pipe = 0; }
    }
}
