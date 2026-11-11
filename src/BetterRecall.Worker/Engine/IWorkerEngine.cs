// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using BetterRecall.Contracts;

namespace BetterRecall.Worker.Engine;

public enum TextKind { Query, Document }

/// <summary>Decoded request image before preprocessing. Bytes are the request payload.</summary>
public sealed record ImageInput(string Format, int Width, int Height, int Stride, ReadOnlyMemory<byte> Bytes);

/// <summary>Vector at the model's native dimension (768). Null when the NSFW gate dropped the image.</summary>
public sealed record ImageEmbedding(float[]? Vector, string? Dropped);

/// <summary>What the protocol handler needs from inference. One call at a time (the worker loop is serial).</summary>
public interface IWorkerEngine : IDisposable
{
    /// <summary>Selects the device on first call (may compile models and take minutes on a cold cache).</summary>
    WorkerHello Hello(int dim);

    /// <summary>nsfwThreshold null skips the classifier. nsfwGrid: 1 whole frame, 2 adds quadrants and a
    /// centre tile, 3 adds a 3x3 grid; the highest score decides. nsfwDetector null skips the detector; otherwise a frame
    /// the classifier passed is dropped when the detector's score reaches it.</summary>
    ImageEmbedding EmbedImage(ImageInput image, double? nsfwThreshold, int nsfwGrid = 1, double? nsfwDetector = null);

    float[] EmbedText(string text, TextKind kind);

    /// <summary>language: "auto" or a Whisper code.</summary>
    TranscribeResult Transcribe(float[] pcm16kMono, string source, string language);

    /// <summary>Re-runs the device benchmark and rewrites cache\device.json.</summary>
    BenchResult Bench();
}
