// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using BetterRecall.Contracts;

namespace BetterRecall.Agent.Worker;

internal sealed record ImageEmbedResult(float[]? Vector, bool DroppedNsfw, double Ms);

/// <summary>Typed worker operations on top of the supervisor. Starts the worker on demand.</summary>
internal sealed class WorkerClient(WorkerSupervisor supervisor)
{
    public WorkerSupervisor Supervisor => supervisor;

    private async Task<WorkerReply> CallAsync(WorkerRequest req, ReadOnlyMemory<byte> payload, CancellationToken ct)
    {
        var conn = await supervisor.EnsureAsync(ct).ConfigureAwait(false);
        var reply = await conn.SendAsync(req, payload, ct).ConfigureAwait(false);
        if (!reply.Header.Ok) throw new WorkerException(reply.Header.Error ?? "Internal error.");
        supervisor.MarkHealthy();
        return reply;
    }

    public Task PrewarmAsync(CancellationToken ct) => supervisor.EnsureAsync(ct);

    /// <summary>nsfwThreshold null skips the NSFW gate; nsfwDetector null skips its second model (the detector).</summary>
    public async Task<ImageEmbedResult> EmbedImageAsync(byte[] image, string mime, double? nsfwThreshold, int nsfwGrid, double? nsfwDetector, CancellationToken ct)
    {
        string format = mime switch
        {
            "image/webp" => "webp",
            "image/jpeg" => "jpeg",
            "image/png" => "png",
            _ => throw new ArgumentException("Unsupported image type."),
        };
        var reply = await CallAsync(new WorkerRequest(0, "embed_image", Format: format, Nsfw: nsfwThreshold, NsfwGrid: nsfwGrid, NsfwDetector: nsfwDetector), image, ct).ConfigureAwait(false);
        if (reply.Header.Dropped == "nsfw") return new ImageEmbedResult(null, true, reply.Header.Ms ?? 0);
        return new ImageEmbedResult(BinaryFrame.BytesToFloats(reply.Payload), false, reply.Header.Ms ?? 0);
    }

    public async Task<float[]> EmbedTextAsync(string text, string kind, CancellationToken ct)
    {
        var reply = await CallAsync(new WorkerRequest(0, "embed_text", Text: text, Kind: kind), ReadOnlyMemory<byte>.Empty, ct).ConfigureAwait(false);
        return BinaryFrame.BytesToFloats(reply.Payload);
    }

    public async Task<TranscribeResult> TranscribeAsync(float[] pcm16kMono, string source, string language, CancellationToken ct)
    {
        var bytes = BinaryFrame.FloatsToBytes(pcm16kMono);
        try
        {
            var reply = await CallAsync(new WorkerRequest(0, "transcribe", SampleRate: 16000, Channels: 1, Format: "f32",
                Source: source, Language: language), bytes, ct).ConfigureAwait(false);
            return reply.Header.Transcript ?? throw new WorkerException("Internal error.");
        }
        finally { Array.Clear(bytes); }
    }

    public async Task<BenchResult> BenchAsync(CancellationToken ct)
    {
        var reply = await CallAsync(new WorkerRequest(0, "bench"), ReadOnlyMemory<byte>.Empty, ct).ConfigureAwait(false);
        return reply.Header.Bench ?? throw new WorkerException("Internal error.");
    }
}
