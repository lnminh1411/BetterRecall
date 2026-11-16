// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Diagnostics;
using System.Reflection;
using System.Text;
using System.Text.Json;
using BetterRecall.Contracts;
using BetterRecall.Worker.Engine;
using BetterRecall.Worker.Inference;
using BetterRecall.Worker.Inference.Onnx;
using BetterRecall.Worker.Models;
using BetterRecall.Worker.Models.Ner;
using BetterRecall.Worker.Protocol;

namespace BetterRecall.Worker.Tests;

/// <summary>Bug sweep 3, batch K (reliability): worker items C9, C11, C13, C14.</summary>
public class Sweep3KWorkerTests
{
    private static T Field<T>(object obj, string name)
        => (T)obj.GetType().GetField(name, BindingFlags.NonPublic | BindingFlags.Instance)!.GetValue(obj)!;

    private static string WriteTokenizer(string vocabJson, string mergesJson)
    {
        var json = $$"""
        {
          "added_tokens": [ {"id":0,"content":"<pad>","special":true}, {"id":1,"content":"<eos>","special":true},
                            {"id":2,"content":"<bos>","special":true}, {"id":3,"content":"<unk>","special":true} ],
          "model": { "type": "BPE", "byte_fallback": true, "vocab": {{vocabJson}}, "merges": {{mergesJson}} }
        }
        """;
        var path = Path.Combine(TestEnv.NewDir("bpe3"), "tokenizer.json");
        File.WriteAllText(path, json, new UTF8Encoding(false));
        return path;
    }

    /// <summary>Rescans the symbol list per merge (the old algorithm): lowest rank first, leftmost on ties.</summary>
    private static List<int> NaiveMerge(List<int> ids, Dictionary<(int, int), (int Rank, int Id)> merges)
    {
        var s = new List<int>(ids);
        while (s.Count > 1)
        {
            int at = -1, rank = int.MaxValue, id = 0;
            for (int k = 0; k + 1 < s.Count; k++)
                if (merges.TryGetValue((s[k], s[k + 1]), out var m) && m.Rank < rank) { rank = m.Rank; at = k; id = m.Id; }
            if (at < 0) break;
            s[at] = id;
            s.RemoveAt(at + 1);
        }
        return s;
    }

    /// <summary>C9: the priority-queue merge gives exactly the rescanning algorithm's tokens on random words over a
    /// vocabulary with overlapping, competing merges.</summary>
    [Fact]
    public void Bpe_merge_matches_the_rescanning_algorithm()
    {
        var letters = "abcd";
        var vocab = new Dictionary<string, int> { ["<pad>"] = 0, ["<eos>"] = 1, ["<bos>"] = 2, ["<unk>"] = 3, ["▁"] = 4 };
        foreach (var c in letters) vocab[c.ToString()] = vocab.Count;
        var rnd = new Random(7);
        var mergeList = new List<(string A, string B)>();
        var pieces = vocab.Keys.Where(k => k.Length == 1 && k != "▁").Append("▁").ToList();
        for (int i = 0; i < 40; i++)
        {
            var a = pieces[rnd.Next(pieces.Count)];
            var b = pieces[rnd.Next(pieces.Count)];
            if (b == "▁" || vocab.ContainsKey(a + b) || a.Length + b.Length > 6) continue;
            vocab[a + b] = vocab.Count;
            mergeList.Add((a, b));
            pieces.Add(a + b);
        }
        var path = WriteTokenizer(JsonSerializer.Serialize(vocab), JsonSerializer.Serialize(mergeList.Select(m => new[] { m.A, m.B })));
        var tok = BpeTokenizer.Load(path);
        var merges = new Dictionary<(int, int), (int, int)>();
        for (int r = 0; r < mergeList.Count; r++) merges[(vocab[mergeList[r].A], vocab[mergeList[r].B])] = (r, vocab[mergeList[r].A + mergeList[r].B]);
        for (int t = 0; t < 300; t++)
        {
            var word = new string(Enumerable.Range(0, rnd.Next(1, 40)).Select(_ => letters[rnd.Next(letters.Length)]).ToArray());
            var start = new List<int> { vocab["▁"] };
            start.AddRange(word.Select(c => vocab[c.ToString()]));
            Assert.Equal(NaiveMerge(start, merges), tok.Encode(word).Select(x => x.Id).ToList());
        }
    }

    /// <summary>C9: a long word without spaces merges in about n log n. 32 000 chars of one letter with doubling merges
    /// (a+a, aa+aa, ...) was about 32 000 full rescans.</summary>
    [Fact]
    public void Bpe_long_word_is_not_quadratic()
    {
        var vocab = new Dictionary<string, int> { ["<pad>"] = 0, ["<eos>"] = 1, ["<bos>"] = 2, ["<unk>"] = 3, ["▁"] = 4, ["a"] = 5 };
        var merges = new List<string[]>();
        for (string p = "a"; p.Length < 1024; p += p)
        {
            vocab[p + p] = vocab.Count;
            merges.Add([p, p]);
        }
        var tok = BpeTokenizer.Load(WriteTokenizer(JsonSerializer.Serialize(vocab), JsonSerializer.Serialize(merges)));
        var word = new string('a', 32_000);
        var sw = Stopwatch.StartNew();
        var ids = tok.Encode(word);
        Assert.True(sw.ElapsedMilliseconds < 2000, $"{sw.ElapsedMilliseconds} ms");
        Assert.Equal(32_000, ids.Where(x => x.Id != vocab["▁"]).Sum(x => x.End - x.Start));
        Assert.Equal(vocab[new string('a', 1024)], ids[1].Id);
    }

    /// <summary>C11: inference.device auto puts Whisper on the NPU, else the CPU; never on the GPU the embedder policy
    /// would pick when there is no NPU. gpu and cpu are as the user chose.</summary>
    [Fact]
    public void Whisper_auto_skips_the_gpu()
    {
        var cpu = new DeviceInfo(DeviceKind.Cpu, "CPU", "CPU", "OpenVINO");
        var gpu = new DeviceInfo(DeviceKind.Gpu, "GPU.0", "Intel GPU", "OpenVINO");
        var npu = new DeviceInfo(DeviceKind.Npu, "NPU", "Intel NPU", "OpenVINO");
        Assert.Equal(["CPU"], DevicePolicy.WhisperAllowed("auto", [gpu, cpu]).Select(d => d.Id));
        Assert.Equal(["NPU", "CPU"], DevicePolicy.WhisperAllowed("auto", [gpu, npu, cpu]).Select(d => d.Id));
        Assert.Equal(["NPU", "CPU"], DevicePolicy.WhisperAllowed("npu", [gpu, npu, cpu]).Select(d => d.Id));
        Assert.Equal(["GPU.0", "CPU"], DevicePolicy.WhisperAllowed("gpu", [gpu, npu, cpu]).Select(d => d.Id));
        Assert.Equal(["CPU"], DevicePolicy.WhisperAllowed("cpu", [gpu, npu, cpu]).Select(d => d.Id));
    }

    /// <summary>C13: the bench's budget sweep compiles other budgets' vision towers into the kept embedder; they are
    /// dropped afterwards, so the worker holds only the configured graphs.</summary>
    [Fact]
    public void Bench_drops_the_graphs_of_other_budgets()
    {
        var home = TestEnv.NewDir("c13");
        var models = Path.Combine(home, "models");
        TestEnv.InstallEmbeddingGemma(models);
        var dir = ModelCatalog.Dir(models, ModelCatalog.EmbeddingGemma);
        File.Copy(Path.Combine(dir, "vision_70.onnx"), Path.Combine(dir, "vision_140.onnx"));
        File.Copy(Path.Combine(dir, "fusion_70.onnx"), Path.Combine(dir, "fusion_140.onnx"));
        var mpath = Path.Combine(dir, "manifest.json");
        var m = JsonSerializer.Deserialize(File.ReadAllBytes(mpath), WorkerJson.Default.ModelManifest)!;
        var v = m.Vision[0];
        m.Vision.Add(new VisionGraph
        {
            Tokens = 140, File = "vision_140.onnx", Height = v.Height, Width = v.Width, Output = v.Output,
            Fusion = new FusionGraph { File = "fusion_140.onnx", InputIdsInput = v.Fusion!.InputIdsInput, AttentionMaskInput = v.Fusion.AttentionMaskInput, InputIds = v.Fusion.InputIds },
        });
        m.Files.Add(new ManifestFile { File = "vision_140.onnx", Size = new FileInfo(Path.Combine(dir, "vision_140.onnx")).Length });
        m.Files.Add(new ManifestFile { File = "fusion_140.onnx", Size = new FileInfo(Path.Combine(dir, "fusion_140.onnx")).Length });
        File.WriteAllBytes(mpath, JsonSerializer.SerializeToUtf8Bytes(m, WorkerJson.Default.ModelManifest));
        var s = TestEnv.Settings(home, x => { x.Inference.Device = "cpu"; x.Inference.ImageTokens = 70; x.Index.Dim = 512; });
        using var engine = new RealEngine(s, home, new OnnxBackend(cpuOnly: true), new OnnxBackend(cpuOnly: true));
        engine.Hello(512);
        var bench = engine.Bench();
        Assert.Contains(bench.Tokens!, t => t.ImageTokens == 140);   // the sweep ran
        var after = Field<Dictionary<string, IInferenceSession>>(Field<Embedder>(engine, "_embedder"), "_sessions").Keys.ToList();
        Assert.DoesNotContain(after, k => k.Contains("vision_140") || k.Contains("fusion_140"));
        Assert.Contains(after, k => k.Contains("vision_70"));
    }

    /// <summary>C14: an OCR preload failing with something other than a WorkerException (here IndexOutOfRange from a
    /// damaged manifest's empty mean) used to fail the whole hello; the worker never came up.</summary>
    [Fact]
    public void Hello_survives_an_ocr_preload_that_throws_anything()
    {
        var home = TestEnv.NewDir("c14");
        var dir = ModelCatalog.Dir(Path.Combine(home, "models"), ModelCatalog.Ocr);
        Directory.CreateDirectory(dir);
        File.Copy(TestEnv.Data("nsfw.onnxt"), Path.Combine(dir, "det.onnx"));
        File.Copy(TestEnv.Data("nsfw.onnxt"), Path.Combine(dir, "rec-main.onnx"));
        File.WriteAllText(Path.Combine(dir, "dict-main.txt"), "a\nb\n");
        var manifest = new ModelManifest
        {
            Name = ModelCatalog.Ocr, Id = "synthetic/ocr",
            Ocr = new OcrSpec
            {
                Det = new OcrDetSpec { File = "det.onnx", Mean = [] },
                Rec = [new OcrRecSpec { Name = "main", File = "rec-main.onnx", Dict = "dict-main.txt", Languages = ["en", "latin"], Classes = 0 }],
            },
            Files = [.. new[] { "det.onnx", "rec-main.onnx", "dict-main.txt" }.Select(f => new ManifestFile { File = f, Size = new FileInfo(Path.Combine(dir, f)).Length })],
        };
        File.WriteAllBytes(Path.Combine(dir, "manifest.json"), JsonSerializer.SerializeToUtf8Bytes(manifest, WorkerJson.Default.ModelManifest));
        var s = TestEnv.Settings(home, x => { x.Ocr.Enabled = true; x.Ocr.Engine = "onnx"; x.Ocr.Device = "cpu"; x.Ocr.Languages = "en"; x.Privacy.NsfwFilter = false; });
        using var engine = new RealEngine(s, home, new OnnxBackend(cpuOnly: true), new OnnxBackend(cpuOnly: true));
        var handler = new RequestHandler(engine, () => s);
        var reply = handler.Handle(JsonSerializer.SerializeToUtf8Bytes(new WorkerRequest(1, "hello"), ContractsJson.Default.WorkerRequest), []);
        Assert.True(reply.Header.Ok, reply.Header.Error);
        Assert.Null(reply.Header.Hello!.OcrDevice);
    }
}
