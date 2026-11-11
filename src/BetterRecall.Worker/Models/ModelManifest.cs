// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Security.Cryptography;
using System.Text.Json;
using System.Text.Json.Serialization;

namespace BetterRecall.Worker.Models;

/// <summary>models\&lt;name&gt;\manifest.json, written by Scripts\models\*.py next to the exported graphs. The C# side
/// follows it: graph files, tensor names, static shapes, preprocessing, prompts, token ids. Its SHA-256 is the
/// "model hash" in the device and compiled-model cache keys.</summary>
public sealed class ModelManifest
{
    public string Name { get; set; } = "";           // embeddinggemma-2 | siglip2-base | nsfw-classifier | nsfw-detector | silero-vad
    public string Id { get; set; } = "";             // google/embeddinggemma-2
    public string Revision { get; set; } = "";       // upstream commit
    public string Licence { get; set; } = "";
    public string Source { get; set; } = "";
    public int Dim { get; set; }
    public TokenizerSpec? Tokenizer { get; set; }
    /// <summary>Token embedding table kept outside the graphs (EmbeddingGemma 2): text graphs take inputs_embeds.</summary>
    public EmbeddingTableSpec? Embeddings { get; set; }
    public PromptSpec? Prompts { get; set; }
    public List<TextGraph> Text { get; set; } = [];
    public List<VisionGraph> Vision { get; set; } = [];
    public ClassifierSpec? Classifier { get; set; }
    public DetectorSpec? Detector { get; set; }
    public OcrSpec? Ocr { get; set; }
    public NerSpec? Ner { get; set; }
    public List<ManifestFile> Files { get; set; } = [];
    /// <summary>OpenVINO IR file -> ONNX graph of the same model, for ONNX Runtime (DirectML graphics cards, CPU without
    /// OpenVINO). Null or missing entry: the IR file only (ONNX Runtime cannot read it).</summary>
    public Dictionary<string, string>? Onnx { get; set; }
    /// <summary>Set by the loader: SHA-256 of manifest.json bytes, lower-case hex.</summary>
    [JsonIgnore] public string Hash { get; set; } = "";
    [JsonIgnore] public string Dir { get; set; } = "";

    public string PathOf(string file) => Path.Combine(Dir, file);

    /// <summary>The file a backend loads for a graph: its ONNX twin on ONNX Runtime when the manifest has one.</summary>
    public string FileFor(string file, bool onnxRuntime)
        => onnxRuntime && Onnx is not null && Onnx.TryGetValue(file, out var o) ? o : file;

    public static ModelManifest? TryLoad(string dir)
    {
        var path = Path.Combine(dir, "manifest.json");
        if (!File.Exists(path)) return null;
        var bytes = File.ReadAllBytes(path);
        var m = JsonSerializer.Deserialize(bytes, WorkerJson.Default.ModelManifest) ?? throw new InvalidDataException("empty manifest");
        m.Hash = Convert.ToHexString(SHA256.HashData(bytes)).ToLowerInvariant();
        m.Dir = dir;
        return m;
    }

    /// <summary>Every file the manifest lists exists with the listed size.</summary>
    public bool FilesPresent(out string? missing)
    {
        foreach (var f in Files)
        {
            var p = PathOf(f.File);
            var fi = new FileInfo(p);
            if (!fi.Exists || (f.Size > 0 && fi.Length != f.Size)) { missing = f.File; return false; }
        }
        missing = null;
        return true;
    }
}

public sealed class ManifestFile
{
    public string File { get; set; } = "";
    public long Size { get; set; }
    public string Sha256 { get; set; } = "";
    public string? Source { get; set; }
}

public sealed class TokenizerSpec
{
    public string File { get; set; } = "tokenizer.model";   // SentencePiece model proto
    public bool AddBos { get; set; } = true;
    public bool AddEos { get; set; } = true;
    public long BosId { get; set; } = 2;
    public long EosId { get; set; } = 1;
    public long PadId { get; set; } = 0;
    public bool Lowercase { get; set; }
    /// <summary>SigLIP-style: always pad to the graph length (attention mask absent).</summary>
    public bool PadToMax { get; set; }
}

public sealed class PromptSpec
{
    public string Query { get; set; } = "{text}";
    public string Document { get; set; } = "{text}";
}

/// <summary>Text graph with one static sequence length.</summary>
public sealed class TextGraph
{
    public string File { get; set; } = "";
    public int Seq { get; set; }
    public string InputIds { get; set; } = "input_ids";
    public string? AttentionMask { get; set; } = "attention_mask";
    /// <summary>When set, the graph takes token embeddings (from the manifest's table) under this name instead of ids,
    /// and a dynamic graph is reshaped to [1, Seq, dim] before compiling.</summary>
    public string? InputsEmbeds { get; set; }
    public string Output { get; set; } = "embedding";
}

public sealed class EmbeddingTableSpec
{
    public string File { get; set; } = "embed_tokens.bf16";
    public int Rows { get; set; }
    public int Dim { get; set; }
    public string DType { get; set; } = "bf16";          // bf16 | f32, little endian, row-major
    /// <summary>Multiplier applied after lookup (Gemma scales token embeddings by sqrt(hidden)).</summary>
    public float Scale { get; set; } = 1f;
}

/// <summary>Vision graph for one token budget. With Fusion set, the vision output feeds the text transformer graph.</summary>
public sealed class VisionGraph
{
    /// <summary>Token budget (inference.imageTokens: 70, 140, 280).</summary>
    public int Tokens { get; set; }
    /// <summary>Soft tokens the graph actually emits for its fixed frame size (at most Tokens). 0 = Tokens.</summary>
    public int SoftTokens { get; set; }
    public string File { get; set; } = "";
    public string Input { get; set; } = "pixel_values";
    public string Output { get; set; } = "image_embeds";
    public int Height { get; set; }
    public int Width { get; set; }
    /// <summary>Patch form (Gemma 4 / EmbeddingGemma 2): when set, Input takes [1,MaxPatches,PatchSize^2*3] patches and
    /// this input [1,MaxPatches,2] (x, y) positions, padding at (-1,-1); the image keeps its aspect ratio (Fit
    /// "aspect") and the graph's first n output slots are its soft tokens, n = real patches / Pool^2.</summary>
    public string? PositionsInput { get; set; }
    /// <summary>Patch form: input [1,Tokens,PatchBudget] taking the 3x3 average-pooling matrix (1/Pool^2 where a patch
    /// belongs to a soft-token slot), built by the worker; null when the graph pools internally.</summary>
    public string? PoolInput { get; set; }
    public int PatchSize { get; set; } = 16;
    public int Pool { get; set; } = 3;
    /// <summary>0 = Tokens * Pool^2.</summary>
    public int MaxPatches { get; set; }
    public ImagePreprocessSpec Preprocess { get; set; } = new();
    public FusionGraph? Fusion { get; set; }

    public int PatchBudget => MaxPatches > 0 ? MaxPatches : Tokens * Pool * Pool;
}

/// <summary>Second pass of an image: the text transformer over the image soft tokens. Two forms:
/// (a) a graph taking ImageEmbedsInput (+ constant ids/mask); (b) InputsEmbeds set: the text graph File runs on
/// [PrefixIds embeds, soft tokens, SuffixIds embeds], reshaped to that static length.</summary>
public sealed class FusionGraph
{
    public string File { get; set; } = "";
    public string? InputsEmbeds { get; set; }
    public long[] PrefixIds { get; set; } = [];
    public long[] SuffixIds { get; set; } = [];
    public string ImageEmbedsInput { get; set; } = "image_embeds";
    public string? InputIdsInput { get; set; }
    public string? AttentionMaskInput { get; set; }
    /// <summary>Token ids of the image-only sequence (placeholders included), as the official processor builds it.</summary>
    public long[] InputIds { get; set; } = [];
    public string Output { get; set; } = "embedding";
}

public sealed class ImagePreprocessSpec
{
    public string Resample { get; set; } = "bicubic";   // bicubic | bilinear
    public float Rescale { get; set; } = 1f / 255f;
    public float[] Mean { get; set; } = [0.5f, 0.5f, 0.5f];
    public float[] Std { get; set; } = [0.5f, 0.5f, 0.5f];
    /// <summary>stretch (resize to H x W), pad (keep aspect, letterbox with PadValue) or aspect (patch form: keep
    /// aspect, resize to the largest multiple of PatchSize*Pool that fits the patch budget).</summary>
    public string Fit { get; set; } = "stretch";
    public float PadValue { get; set; }
}

public sealed class ClassifierSpec
{
    /// <summary>CPU graph (INT8 dynamic quantisation: MatMulInteger / DynamicQuantizeLinear).</summary>
    public string File { get; set; } = "";
    /// <summary>FP32 graph for the NPU and GPUs, which do not run the CPU's dynamic INT8 ops well. Null: File.</summary>
    public string? AcceleratorFile { get; set; }
    public string Input { get; set; } = "pixel_values";
    public string Output { get; set; } = "logits";
    public int Height { get; set; } = 224;
    public int Width { get; set; } = 224;
    public ImagePreprocessSpec Preprocess { get; set; } = new() { Resample = "bilinear" };
    public string[] Labels { get; set; } = [];
    /// <summary>Labels whose summed softmax probability is the NSFW score.</summary>
    public string[] BlockLabels { get; set; } = [];
}

/// <summary>YOLOv8-style detector (NudeNet): input [1,3,Size,Size] RGB /255, image letterboxed at the top left on a
/// black square; output [1, 4 + Labels, anchors]: rows 0-3 box (cx, cy, w, h), then one sigmoid score per label.
/// Score = highest score of any BlockLabels row over all anchors (no NMS needed).</summary>
public sealed class DetectorSpec
{
    public string File { get; set; } = "";
    public string Input { get; set; } = "images";
    public string Output { get; set; } = "output0";
    public int Size { get; set; } = 640;
    public string[] Labels { get; set; } = [];
    public string[] BlockLabels { get; set; } = [];
}

/// <summary>PaddleOCR (models\ocr, Scripts\models\export_ocr.py): one DB text detector and CTC line recognisers.</summary>
public sealed class OcrSpec
{
    public OcrDetSpec Det { get; set; } = new();
    /// <summary>First entry ("main") always runs; the others when ocr.languages asks for one of their languages.</summary>
    public List<OcrRecSpec> Rec { get; set; } = [];
}

/// <summary>DB detector: input [1,3,H,W] BGR, (x/255 - mean) / std, H and W multiples of 32; output [1,1,H,W]
/// text probability. Thresholds from the upstream inference.yml.</summary>
public sealed class OcrDetSpec
{
    public string File { get; set; } = "det.onnx";
    public string Input { get; set; } = "x";
    public float Thresh { get; set; } = 0.2f;
    public float BoxThresh { get; set; } = 0.45f;
    public float Unclip { get; set; } = 1.4f;
    public float[] Mean { get; set; } = [0.485f, 0.456f, 0.406f];
    public float[] Std { get; set; } = [0.229f, 0.224f, 0.225f];
    /// <summary>Longer image side is scaled down to this before detection.</summary>
    public int MaxSide { get; set; } = 1920;
}

/// <summary>CTC line recogniser: input [N,3,Height,W] BGR, (x/255 - 0.5) / 0.5; output [N,T,Classes] softmax;
/// class 0 = blank, 1..n = Dict lines, n+1 = space.</summary>
public sealed class OcrRecSpec
{
    public string Name { get; set; } = "";
    public string File { get; set; } = "";
    public string Dict { get; set; } = "";
    public string Input { get; set; } = "x";
    public int Height { get; set; } = 48;
    /// <summary>Language codes the model reads; "latin" stands for every Latin-script language.</summary>
    public string[] Languages { get; set; } = [];
    public int Classes { get; set; }
}

/// <summary>Token-classification PII model (models\ner, Scripts\models\export_ner.py): BIO tags over a BPE
/// tokenizer.json; Entities maps upstream base labels to person | address.</summary>
public sealed class NerSpec
{
    public string File { get; set; } = "model.onnx";
    public string Tokenizer { get; set; } = "tokenizer.json";
    public string InputIds { get; set; } = "input_ids";
    public string AttentionMask { get; set; } = "attention_mask";
    public string Output { get; set; } = "logits";
    public int MaxTokens { get; set; } = 512;
    public string[] Labels { get; set; } = [];
    public Dictionary<string, string> Entities { get; set; } = [];
}

[JsonSourceGenerationOptions(
    PropertyNamingPolicy = JsonKnownNamingPolicy.CamelCase,
    DefaultIgnoreCondition = JsonIgnoreCondition.WhenWritingNull,
    WriteIndented = true,
    ReadCommentHandling = JsonCommentHandling.Skip,
    AllowTrailingCommas = true)]
[JsonSerializable(typeof(ModelManifest))]
[JsonSerializable(typeof(Inference.DeviceCacheFile))]
public partial class WorkerJson : JsonSerializerContext;
