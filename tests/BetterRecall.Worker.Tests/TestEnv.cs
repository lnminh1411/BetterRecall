// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Buffers.Binary;
using System.IO.Pipes;
using System.Text;
using System.Text.Json;
using BetterRecall.Contracts;
using BetterRecall.Worker.Engine;
using BetterRecall.Worker.Hosting;
using BetterRecall.Worker.Models;
using BetterRecall.Worker.Protocol;

namespace BetterRecall.Worker.Tests;

internal static class TestEnv
{
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

    public static WorkerSettings Settings(string home, Action<Settings>? edit = null)
    {
        var s = new Settings();
        s.Inference.ModelDir = Path.Combine(home, "models");
        edit?.Invoke(s);
        Directory.CreateDirectory(home);
        File.WriteAllBytes(DataPaths.SettingsFile(home), JsonSerializer.SerializeToUtf8Bytes(s, ContractsJson.Default.Settings));
        return WorkerSettings.From(s, home);
    }

    private static ManifestFile Copy(string dir, string src, string dst)
    {
        Directory.CreateDirectory(dir);
        var to = Path.Combine(dir, dst);
        File.Copy(Data(src), to, overwrite: true);
        return new ManifestFile { File = dst, Size = new FileInfo(to).Length };
    }

    private static void WriteManifest(string dir, ModelManifest m)
        => File.WriteAllBytes(Path.Combine(dir, "manifest.json"), JsonSerializer.SerializeToUtf8Bytes(m, WorkerJson.Default.ModelManifest));

    /// <summary>Synthetic EmbeddingGemma-2-shaped model set: two text buckets, a vision tower and a fusion graph.</summary>
    public static void InstallEmbeddingGemma(string modelDir, string? corruptVision = null)
    {
        var dir = ModelCatalog.Dir(modelDir, ModelCatalog.EmbeddingGemma);
        var files = new List<ManifestFile>
        {
            Copy(dir, "sp.model", "tokenizer.model"),
            Copy(dir, "text_16.onnxt", "text_16.onnx"),
            Copy(dir, "text_32.onnxt", "text_32.onnx"),
            Copy(dir, "vision_28.onnxt", "vision_70.onnx"),
            Copy(dir, "fusion_28.onnxt", "fusion_70.onnx"),
        };
        if (corruptVision is not null) File.WriteAllText(Path.Combine(dir, "vision_70.onnx"), corruptVision);
        files[3].Size = new FileInfo(Path.Combine(dir, "vision_70.onnx")).Length;
        var ids = new long[32];
        ids[0] = 2; for (int i = 1; i < 31; i++) ids[i] = 300; ids[31] = 1;
        WriteManifest(dir, new ModelManifest
        {
            Name = ModelCatalog.EmbeddingGemma, Id = "synthetic/embeddinggemma-2", Revision = "syn1", Dim = 768,
            Tokenizer = new TokenizerSpec { File = "tokenizer.model", AddBos = true, AddEos = true, BosId = 2, EosId = 1, PadId = 0 },
            Prompts = new PromptSpec { Query = "task: search result | query: {text}", Document = "title: none | text: {text}" },
            Text = [new TextGraph { File = "text_16.onnx", Seq = 16 }, new TextGraph { File = "text_32.onnx", Seq = 32 }],
            Vision =
            [
                new VisionGraph
                {
                    Tokens = 70, File = "vision_70.onnx", Height = 64, Width = 112, Output = "image_embeds",
                    Fusion = new FusionGraph { File = "fusion_70.onnx", InputIdsInput = "input_ids", AttentionMaskInput = "attention_mask", InputIds = ids },
                },
            ],
            Files = files,
        });
    }

    /// <summary>The real EmbeddingGemma 2 layout: bf16 token table, one dynamic text graph over inputs_embeds (compiled
    /// per static length), vision tower whose soft tokens go through the same text graph.</summary>
    public static void InstallEmbeddingGemmaTableLayout(string modelDir)
    {
        var dir = ModelCatalog.Dir(modelDir, ModelCatalog.EmbeddingGemma);
        var files = new List<ManifestFile>
        {
            Copy(dir, "sp.model", "tokenizer.model"),
            Copy(dir, "embed_tokens.bf16", "embed_tokens.bf16"),
            Copy(dir, "text_dyn.onnxt", "text.onnx"),
            Copy(dir, "vision_28.onnxt", "vision_140.onnx"),
        };
        WriteManifest(dir, new ModelManifest
        {
            Name = ModelCatalog.EmbeddingGemma, Id = "synthetic/embeddinggemma-2-table", Revision = "syn1", Dim = 768,
            Tokenizer = new TokenizerSpec { File = "tokenizer.model", AddBos = true, AddEos = true, BosId = 2, EosId = 1, PadId = 0 },
            Embeddings = new EmbeddingTableSpec { File = "embed_tokens.bf16", Rows = 320, Dim = 32, DType = "bf16", Scale = 1.5f },
            Prompts = new PromptSpec { Query = "task: search result | query: {text}", Document = "title: none | text: {text}" },
            Text =
            [
                new TextGraph { File = "text.onnx", Seq = 16, InputsEmbeds = "inputs_embeds" },
                new TextGraph { File = "text.onnx", Seq = 64, InputsEmbeds = "inputs_embeds" },
            ],
            Vision =
            [
                new VisionGraph
                {
                    Tokens = 28, File = "vision_140.onnx", Height = 64, Width = 112, Output = "image_embeds",
                    Preprocess = new ImagePreprocessSpec { Resample = "bicubic", Rescale = 1f / 255f, Mean = [0, 0, 0], Std = [1, 1, 1] },
                    Fusion = new FusionGraph
                    {
                        File = "text.onnx", InputsEmbeds = "inputs_embeds", AttentionMaskInput = "attention_mask",
                        PrefixIds = [2, 300], SuffixIds = [301, 1],
                    },
                },
            ],
            Files = files,
        });
    }

    public static void InstallSiglip(string modelDir)
    {
        var dir = ModelCatalog.Dir(modelDir, ModelCatalog.Siglip);
        var files = new List<ManifestFile>
        {
            Copy(dir, "sp.model", "tokenizer.model"),
            Copy(dir, "text_16.onnxt", "text.onnx"),
            Copy(dir, "siglip_vision.onnxt", "vision.onnx"),
        };
        WriteManifest(dir, new ModelManifest
        {
            Name = ModelCatalog.Siglip, Id = "synthetic/siglip2-base", Revision = "syn1", Dim = 768,
            Tokenizer = new TokenizerSpec { AddBos = false, AddEos = true, EosId = 1, PadId = 0, PadToMax = true, Lowercase = true },
            Text = [new TextGraph { File = "text.onnx", Seq = 16 }],
            Vision = [new VisionGraph { Tokens = 196, File = "vision.onnx", Height = 64, Width = 64, Output = "embedding" }],
            Files = files,
        });
    }

    public static void InstallNsfw(string modelDir)
    {
        var dir = ModelCatalog.Dir(modelDir, ModelCatalog.Nsfw);
        var f = Copy(dir, "nsfw.onnxt", "model_int8.onnx");
        WriteManifest(dir, new ModelManifest
        {
            Name = ModelCatalog.Nsfw, Id = "synthetic/nsfw",
            Classifier = new ClassifierSpec { File = "model_int8.onnx", Output = "logits", Labels = ["normal", "nsfw"], BlockLabels = ["nsfw"] },
            Files = [f],
        });
    }

    /// <summary>NudeNet-shaped detector whose every score is the mean green of the frame (green frame: 1).</summary>
    public static void InstallNsfwDetector(string modelDir)
    {
        var dir = ModelCatalog.Dir(modelDir, ModelCatalog.NsfwDetector);
        var f = Copy(dir, "nsfw_detector.onnxt", "model.onnx");
        WriteManifest(dir, new ModelManifest
        {
            Name = ModelCatalog.NsfwDetector, Id = "synthetic/nsfw-detector",
            Detector = new DetectorSpec { File = "model.onnx", Size = 64, Labels = [.. NudeNetLabels], BlockLabels = ["FEMALE_BREAST_EXPOSED", "BUTTOCKS_COVERED"] },
            Files = [f],
        });
    }

    public static readonly string[] NudeNetLabels =
    [
        "FEMALE_GENITALIA_COVERED", "FACE_FEMALE", "BUTTOCKS_EXPOSED", "FEMALE_BREAST_EXPOSED", "FEMALE_GENITALIA_EXPOSED",
        "MALE_BREAST_EXPOSED", "ANUS_EXPOSED", "FEET_EXPOSED", "BELLY_COVERED", "FEET_COVERED", "ARMPITS_COVERED",
        "ARMPITS_EXPOSED", "FACE_MALE", "BELLY_EXPOSED", "MALE_GENITALIA_EXPOSED", "ANUS_COVERED", "FEMALE_BREAST_COVERED",
        "BUTTOCKS_COVERED",
    ];

    public static void InstallVad(string modelDir)
        => Copy(ModelCatalog.Dir(modelDir, ModelCatalog.Vad), "vad.onnxt", ModelCatalog.VadFile);

    public static byte[] Bgra(int w, int h, byte r, byte g, byte b)
    {
        var p = new byte[w * h * 4];
        for (int i = 0; i < w * h; i++) { p[i * 4] = b; p[i * 4 + 1] = g; p[i * 4 + 2] = r; p[i * 4 + 3] = 255; }
        return p;
    }

    public static byte[] Pcm(float amplitude, int samples = 16000)
    {
        var pcm = new float[samples];
        for (int i = 0; i < pcm.Length; i++) pcm[i] = amplitude * MathF.Sin(i * 0.05f);
        return BinaryFrame.FloatsToBytes(pcm);
    }

    public static float[] ReadWav16(string path)
    {
        using var fs = File.OpenRead(path);
        using var reader = new BinaryReader(fs);
        if (fs.Length < 12) throw new InvalidDataException("File too short for WAV header.");
        var riff = reader.ReadBytes(4);
        if (Encoding.ASCII.GetString(riff) != "RIFF") throw new InvalidDataException("Not a RIFF file.");
        reader.ReadUInt32();
        var wave = reader.ReadBytes(4);
        if (Encoding.ASCII.GetString(wave) != "WAVE") throw new InvalidDataException("Not a WAVE file.");

        ushort format = 0;
        ushort channels = 0;
        uint sampleRate = 0;
        ushort bitsPerSample = 0;
        bool hasFmt = false;
        byte[]? dataBytes = null;

        while (fs.Position < fs.Length)
        {
            if (fs.Position + 8 > fs.Length) break;
            var chunkIdBytes = reader.ReadBytes(4);
            string chunkId = Encoding.ASCII.GetString(chunkIdBytes);
            uint chunkSize = reader.ReadUInt32();

            if (chunkId == "fmt ")
            {
                if (chunkSize < 16) throw new InvalidDataException($"'fmt ' chunk too small: {chunkSize}");
                format = reader.ReadUInt16();
                channels = reader.ReadUInt16();
                sampleRate = reader.ReadUInt32();
                reader.ReadUInt32();
                reader.ReadUInt16();
                bitsPerSample = reader.ReadUInt16();
                hasFmt = true;
                if (chunkSize > 16)
                {
                    reader.BaseStream.Seek((long)(chunkSize - 16), SeekOrigin.Current);
                }
            }
            else if (chunkId == "data")
            {
                dataBytes = reader.ReadBytes((int)chunkSize);
            }
            else
            {
                reader.BaseStream.Seek((long)chunkSize, SeekOrigin.Current);
            }

            if ((chunkSize & 1) != 0 && fs.Position < fs.Length)
            {
                reader.BaseStream.Seek(1, SeekOrigin.Current);
            }
        }

        if (!hasFmt) throw new InvalidDataException("Missing 'fmt ' chunk in WAV.");
        if (dataBytes is null) throw new InvalidDataException("Missing 'data' chunk in WAV.");
        if (format != 1 || bitsPerSample != 16 || channels != 1 || sampleRate != 16000)
        {
            throw new InvalidDataException($"Expected PCM format 1, 16 bits, 1 channel, 16000 Hz, but got format={format}, bits={bitsPerSample}, channels={channels}, rate={sampleRate}.");
        }

        int sampleCount = dataBytes.Length / 2;
        var samples = new float[sampleCount];
        for (int i = 0; i < sampleCount; i++)
        {
            short s = BinaryPrimitives.ReadInt16LittleEndian(dataBytes.AsSpan(i * 2, 2));
            samples[i] = s / 32768f;
        }
        return samples;
    }

    public static byte[] ToBgra(RgbImage img)
    {
        var bgra = new byte[img.Width * img.Height * 4];
        var rgb = img.Rgb;
        for (int i = 0; i < img.Width * img.Height; i++)
        {
            bgra[i * 4 + 0] = rgb[i * 3 + 2];
            bgra[i * 4 + 1] = rgb[i * 3 + 1];
            bgra[i * 4 + 2] = rgb[i * 3 + 0];
            bgra[i * 4 + 3] = 255;
        }
        return bgra;
    }
}

/// <summary>Real named pipe: the test is the agent (server), the worker loop runs in-process as the client.</summary>
internal sealed class PipeHarness : IAsyncDisposable
{
    public NamedPipeServerStream Server { get; }
    public NamedPipeClientStream Client { get; }
    public Task<LoopExit> Loop { get; }
    private long _id;

    private PipeHarness(NamedPipeServerStream server, NamedPipeClientStream client, Task<LoopExit> loop)
    { Server = server; Client = client; Loop = loop; }

    public static async Task<PipeHarness> StartAsync(IWorkerEngine engine, WorkerSettings settings, TimeSpan? idle = null)
    {
        var name = PipeNames.NewWorker();
        var server = new NamedPipeServerStream(name, PipeDirection.InOut, 1, PipeTransmissionMode.Byte, PipeOptions.Asynchronous);
        var wait = server.WaitForConnectionAsync();
        var client = await AgentPipe.ConnectAsync(name, TimeSpan.FromSeconds(5), CancellationToken.None);
        await wait;
        var handler = new RequestHandler(engine, () => settings);
        var loop = Task.Run(() => WorkerLoop.RunAsync(client, handler, idle ?? TimeSpan.FromSeconds(60), CancellationToken.None));
        return new PipeHarness(server, client, loop);
    }

    public Task<(WorkerResponse Header, byte[] Payload)> CallAsync(WorkerRequest r, byte[]? payload = null, TimeSpan? timeout = null)
        => CallRawAsync(JsonSerializer.SerializeToUtf8Bytes(r with { Id = r.Id == 0 ? ++_id : r.Id }, ContractsJson.Default.WorkerRequest), payload ?? [], timeout);

    public async Task<(WorkerResponse Header, byte[] Payload)> CallRawAsync(byte[] header, byte[] payload, TimeSpan? timeout = null)
    {
        using var cts = new CancellationTokenSource(timeout ?? TimeSpan.FromSeconds(120));
        await BinaryFrame.WriteAsync(Server, header, payload, cts.Token);
        var f = await BinaryFrame.ReadAsync(Server, cts.Token) ?? throw new EndOfStreamException();
        return (JsonSerializer.Deserialize(f.Header, ContractsJson.Default.WorkerResponse)!, f.Payload);
    }

    public async ValueTask DisposeAsync()
    {
        await Server.DisposeAsync();
        try { await Loop.WaitAsync(TimeSpan.FromSeconds(10)); } catch { }
        await Client.DisposeAsync();
    }
}
