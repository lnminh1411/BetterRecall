// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Text.Json;
using System.Text.Json.Serialization;
using BetterRecall.Contracts;

namespace BetterRecall.Worker.Tests;

internal static class TestEnv
{
    private static readonly JsonSerializerOptions ManifestJson = new()
    {
        PropertyNamingPolicy = JsonNamingPolicy.CamelCase,
        DefaultIgnoreCondition = JsonIgnoreCondition.WhenWritingNull,
    };

    /// <summary>BR_TEST_TMP (set by a local runsettings file) plus "betterrecall-worker", else the temp dir.</summary>
    public static string Root
    {
        get
        {
            var env = Environment.GetEnvironmentVariable("BR_TEST_TMP");
            return Path.Combine(string.IsNullOrWhiteSpace(env) ? Path.GetTempPath() : env, "betterrecall-worker");
        }
    }

    private static int _initialized;

    /// <summary>A fresh folder under <see cref="Root"/>. The test host does not run exit handlers, so the first call of
    /// a run sweeps folders earlier runs left behind once they are 30 minutes old (runs sharing Root take seconds to a few
    /// minutes). Tests that copy a whole install (~500 MB) use <see cref="Scoped"/> and delete their folder at once.</summary>
    public static string NewDir(string tag)
    {
        if (Interlocked.Exchange(ref _initialized, 1) == 0) SweepStale(TimeSpan.FromMinutes(30));
        var d = Path.Combine(Root, $"{tag}-{Guid.NewGuid():N}"[..(tag.Length + 13)]);
        Directory.CreateDirectory(d);
        return d;
    }

    /// <summary>NewDir that is deleted when disposed (best effort: a child process still holding a file leaves it to the sweep).</summary>
    public static ScopedDir Scoped(string tag) => new(NewDir(tag));

    internal sealed class ScopedDir(string path) : IDisposable
    {
        public string Path { get; } = path;
        public void Dispose() => TryDelete(Path);
    }

    /// <summary>True for a folder name NewDir makes: "&lt;tag&gt;-" plus 12 hex digits.</summary>
    internal static bool IsTestDirName(string name)
    {
        int dash = name.LastIndexOf('-');
        return dash > 0 && name.Length - dash - 1 == 12 && name[(dash + 1)..].All(char.IsAsciiHexDigitLower);
    }

    private static void TryDelete(string d)
    {
        try { if (Directory.Exists(d)) Directory.Delete(d, recursive: true); }
        catch (IOException) { }          // a worker child still holds a file; the stale sweep gets it later
        catch (UnauthorizedAccessException) { }
    }

    private static void SweepStale(TimeSpan age)
    {
        try
        {
            if (!Directory.Exists(Root)) return;
            var cutoff = DateTime.UtcNow - age;
            foreach (var d in Directory.EnumerateDirectories(Root))
            {
                if (!IsTestDirName(Path.GetFileName(d)) || Directory.GetLastWriteTimeUtc(d) > cutoff) continue;
                TryDelete(d);
            }
        }
        catch (IOException) { }
    }

    public static string Data(string file) => Path.Combine(AppContext.BaseDirectory, "TestData", file);

    public static void Settings(string home, Action<Settings>? edit = null)
    {
        var s = new Settings();
        s.Inference.ModelDir = Path.Combine(home, "models");
        edit?.Invoke(s);
        Directory.CreateDirectory(home);
        File.WriteAllBytes(DataPaths.SettingsFile(home), JsonSerializer.SerializeToUtf8Bytes(s, ContractsJson.Default.Settings));
    }

    private static object CopyFile(string dir, string src, string dst)
    {
        Directory.CreateDirectory(dir);
        var to = Path.Combine(dir, dst);
        File.Copy(Data(src), to, overwrite: true);
        return new { file = dst, size = new FileInfo(to).Length };
    }

    private static void WriteManifest(string dir, object manifest)
        => File.WriteAllBytes(Path.Combine(dir, "manifest.json"), JsonSerializer.SerializeToUtf8Bytes(manifest, ManifestJson));

    /// <summary>Synthetic EmbeddingGemma-2-shaped model set: two text buckets, a vision tower and a fusion graph.</summary>
    public static void InstallEmbeddingGemma(string modelDir)
    {
        const string name = "embeddinggemma-2";
        var dir = Path.Combine(modelDir, name);
        object[] files =
        [
            CopyFile(dir, "sp.model", "tokenizer.model"),
            CopyFile(dir, "text_16.onnxt", "text_16.onnx"),
            CopyFile(dir, "text_32.onnxt", "text_32.onnx"),
            CopyFile(dir, "vision_28.onnxt", "vision_70.onnx"),
            CopyFile(dir, "fusion_28.onnxt", "fusion_70.onnx"),
        ];
        var ids = new long[32];
        ids[0] = 2; for (int i = 1; i < 31; i++) ids[i] = 300; ids[31] = 1;
        WriteManifest(dir, new
        {
            name,
            id = "synthetic/embeddinggemma-2",
            revision = "syn1",
            dim = 768,
            tokenizer = new { file = "tokenizer.model", addBos = true, addEos = true, bosId = 2L, eosId = 1L, padId = 0L },
            prompts = new { query = "task: search result | query: {text}", document = "title: none | text: {text}" },
            text = new object[]
            {
                new { file = "text_16.onnx", seq = 16 },
                new { file = "text_32.onnx", seq = 32 },
            },
            vision = new object[]
            {
                new
                {
                    tokens = 70,
                    file = "vision_70.onnx",
                    height = 64,
                    width = 112,
                    output = "image_embeds",
                    fusion = new { file = "fusion_70.onnx", inputIdsInput = "input_ids", attentionMaskInput = "attention_mask", inputIds = ids },
                },
            },
            files,
        });
    }

    public static void InstallNsfw(string modelDir)
    {
        const string name = "nsfw-classifier";
        var dir = Path.Combine(modelDir, name);
        var f = CopyFile(dir, "nsfw.onnxt", "model_int8.onnx");
        WriteManifest(dir, new
        {
            name,
            id = "synthetic/nsfw",
            classifier = new { file = "model_int8.onnx", output = "logits", labels = new[] { "normal", "nsfw" }, blockLabels = new[] { "nsfw" } },
            files = new[] { f },
        });
    }

    public static void InstallVad(string modelDir)
        => CopyFile(Path.Combine(modelDir, "silero-vad"), "vad.onnxt", "silero_vad.onnx");
}
