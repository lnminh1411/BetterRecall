// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Buffers.Binary;
using System.Diagnostics;
using System.Globalization;
using System.Security.Cryptography;
using System.Text;
using BetterRecall.Contracts;
using BetterRecall.Worker.Vectors;

namespace BetterRecall.Worker.Engine;

/// <summary>inference.fake: no model files, deterministic output.
/// Text: bag of hashed lower-case words plus a small kind-specific offset, so the same text gives the same vector,
/// a query and a document of the same text have cosine about 0.99, and texts sharing words are closer than unrelated
/// ones. Images: hash of the pixels' bytes and shape. Transcribe: echo of size and source, Speech when RMS > 1e-3.</summary>
public sealed class FakeEngine : IWorkerEngine
{
    public const int NativeDim = 768;
    public const string ModelId = "fake/embeddinggemma-2";
    public const string Revision = "fake-1";
    private const float KindWeight = 0.12f;

    public WorkerHello Hello(int dim) => new(1, "CPU", "fake", ModelId, Revision, dim, Asr: true, Vad: true, Missing: []);

    public ImageEmbedding EmbedImage(ImageInput image, double? nsfwThreshold, int nsfwGrid = 1, double? nsfwDetector = null)
    {
        using var h = IncrementalHash.CreateHash(HashAlgorithmName.SHA256);
        h.AppendData(Encoding.UTF8.GetBytes($"img|{image.Format}|{image.Width}x{image.Height}|"));
        h.AppendData(image.Bytes.Span);
        var v = HashVector(h.GetHashAndReset());
        VectorMath.L2NormalizeInPlace(v);
        return new ImageEmbedding(v, null);
    }

    public float[] EmbedText(string text, TextKind kind)
    {
        var v = new float[NativeDim];
        int words = 0;
        foreach (var w in Words(text))
        {
            var wv = HashVector(SHA256.HashData(Encoding.UTF8.GetBytes("w|" + w)));
            for (int i = 0; i < v.Length; i++) v[i] += wv[i];
            words++;
        }
        if (words == 0) v = HashVector(SHA256.HashData(Encoding.UTF8.GetBytes("empty")));
        VectorMath.L2NormalizeInPlace(v);
        var k = HashVector(SHA256.HashData(Encoding.UTF8.GetBytes(kind == TextKind.Query ? "kind|query" : "kind|document")));
        VectorMath.L2NormalizeInPlace(k);
        for (int i = 0; i < v.Length; i++) v[i] += KindWeight * k[i];
        VectorMath.L2NormalizeInPlace(v);
        return v;
    }

    public TranscribeResult Transcribe(float[] pcm, string source, string language)
    {
        var sw = Stopwatch.StartNew();
        double sum = 0;
        foreach (var x in pcm) sum += (double)x * x;
        double rms = pcm.Length == 0 ? 0 : Math.Sqrt(sum / pcm.Length);
        int ms = (int)(pcm.Length * 1000L / 16000);
        string lang = language == "auto" ? "en" : language;
        if (rms <= 1e-3)
            return new TranscribeResult(false, "", lang, [], sw.ElapsedMilliseconds);
        var hash = Convert.ToHexString(SHA256.HashData(MemoryMarshalBytes(pcm)))[..8].ToLowerInvariant();
        var text = string.Create(CultureInfo.InvariantCulture, $"fake transcript {source} {ms} ms {hash}");
        return new TranscribeResult(true, text, lang, [new TranscriptSegment(0, ms, text)], sw.ElapsedMilliseconds);
    }

    public BenchResult Bench() => new("CPU", [new BenchDevice("CPU", "fake", true, 0, 0, 1.0, null)]);

    public void Dispose() { }

    internal static IEnumerable<string> Words(string text)
    {
        var sb = new StringBuilder();
        foreach (var ch in text.Normalize(NormalizationForm.FormKC))
        {
            if (char.IsLetterOrDigit(ch)) sb.Append(char.ToLowerInvariant(ch));
            else if (sb.Length > 0) { yield return sb.ToString(); sb.Clear(); }
        }
        if (sb.Length > 0) yield return sb.ToString();
    }

    /// <summary>768 floats in [-1, 1) from a seed, via SHA-256 in counter mode.</summary>
    internal static float[] HashVector(byte[] seed)
    {
        var v = new float[NativeDim];
        var block = new byte[seed.Length + 4];
        seed.CopyTo(block, 0);
        int filled = 0;
        for (uint ctr = 0; filled < v.Length; ctr++)
        {
            BinaryPrimitives.WriteUInt32LittleEndian(block.AsSpan(seed.Length), ctr);
            var d = SHA256.HashData(block);
            for (int j = 0; j + 1 < d.Length && filled < v.Length; j += 2)
                v[filled++] = (BinaryPrimitives.ReadInt16LittleEndian(d.AsSpan(j)) / 32768f);
        }
        return v;
    }

    private static byte[] MemoryMarshalBytes(float[] pcm)
    {
        var b = new byte[pcm.Length * 4];
        Buffer.BlockCopy(pcm, 0, b, 0, b.Length);
        return b;
    }
}
