// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Buffers.Binary;
using System.Diagnostics;
using System.Text.Json;
using BetterRecall.Contracts;
using BetterRecall.Worker.Engine;
using BetterRecall.Worker.Hosting;
using BetterRecall.Worker.Vectors;

namespace BetterRecall.Worker.Protocol;

/// <summary>One decoded response: header plus optional payload.</summary>
public readonly record struct WorkerReply(WorkerResponse Header, byte[] Payload, bool Shutdown = false);

/// <summary>Validates one request, calls the engine, maps exceptions to the section 4 error strings.</summary>
public sealed class RequestHandler(IWorkerEngine engine, Func<WorkerSettings> settings)
{
    public static readonly string[] ImageFormats = ["webp", "jpeg", "png", "bgra8"];
    public const int MaxImageSide = 16384;

    public WorkerReply Handle(ReadOnlySpan<byte> headerBytes, byte[] payload)
    {
        WorkerRequest? req;
        try
        {
            req = JsonSerializer.Deserialize(headerBytes, ContractsJson.Default.WorkerRequest);
        }
        catch (JsonException)
        {
            return Fail(TryReadId(headerBytes), WorkerErrors.BadRequest, "header json");
        }
        if (req is null || string.IsNullOrEmpty(req.Op)) return Fail(TryReadId(headerBytes), WorkerErrors.BadRequest, "no op");

        var sw = Stopwatch.StartNew();
        try
        {
            var reply = Dispatch(req, payload, sw);
            Log.Debug($"op={req.Op} id={req.Id} ok={reply.Header.Ok} in={payload.Length}B ms={sw.Elapsed.TotalMilliseconds:F1}");
            return reply;
        }
        catch (WorkerException e)
        {
            Log.Info($"op={req.Op} id={req.Id} failed: {e.Message}{(e is BadRequestException b && b.Why is not null ? " (" + b.Why + ")" : "")}");
            return new WorkerReply(new WorkerResponse(req.Id, false, e.Message), []);
        }
        catch (Exception e)
        {
            Log.Error($"op={req.Op} id={req.Id} internal: {e.GetType().FullName}: {e.Message}");
            return new WorkerReply(new WorkerResponse(req.Id, false, WorkerErrors.Internal), []);
        }
    }

    private WorkerReply Dispatch(WorkerRequest r, byte[] payload, Stopwatch sw)
    {
        var s = settings();
        switch (r.Op)
        {
            case "hello":
                return new WorkerReply(new WorkerResponse(r.Id, true, Hello: engine.Hello(s.Dim)), []);

            case "embed_image":
            {
                var image = ValidateImage(r, payload);
                if (r.Nsfw is { } t && (double.IsNaN(t) || t <= 0 || t > 1)) throw new BadRequestException("nsfw range");
                if (r.NsfwGrid is < 1 or > 3) throw new BadRequestException("nsfw grid");
                if (r.NsfwDetector is { } dt && (double.IsNaN(dt) || dt <= 0 || dt > 1)) throw new BadRequestException("nsfw detector range");
                var result = engine.EmbedImage(image, r.Nsfw, r.NsfwGrid ?? 1, r.NsfwDetector);
                if (result.Dropped is not null)
                    return new WorkerReply(new WorkerResponse(r.Id, true, Ms: Ms(sw), Dropped: result.Dropped), []);
                var v = Finish(result.Vector!, s.Dim);
                return new WorkerReply(new WorkerResponse(r.Id, true, Dim: v.Length, Ms: Ms(sw)), BinaryFrame.FloatsToBytes(v));
            }

            case "embed_text":
            {
                if (r.Text is null) throw new BadRequestException("text missing");
                var kind = r.Kind switch
                {
                    "query" => TextKind.Query,
                    "document" => TextKind.Document,
                    _ => throw new BadRequestException("kind"),
                };
                var v = Finish(engine.EmbedText(r.Text, kind), s.Dim);
                return new WorkerReply(new WorkerResponse(r.Id, true, Dim: v.Length, Ms: Ms(sw)), BinaryFrame.FloatsToBytes(v));
            }

            case "transcribe":
            {
                if (r.SampleRate != 16000 || r.Channels != 1 || r.Format != "f32") throw new BadRequestException("audio format");
                if (r.Source is not ("system" or "mic")) throw new BadRequestException("source");
                if (payload.Length == 0 || payload.Length % 4 != 0) throw new BadRequestException("pcm length");
                string lang = string.IsNullOrWhiteSpace(r.Language) ? s.AudioLanguage : r.Language.Trim().ToLowerInvariant();
                if (!IsLanguage(lang)) throw new BadRequestException("language");
                var pcm = new float[payload.Length / 4];
                for (int i = 0; i < pcm.Length; i++)
                {
                    float x = BinaryPrimitives.ReadSingleLittleEndian(payload.AsSpan(i * 4));
                    if (!float.IsFinite(x)) throw new BadRequestException("pcm not finite");
                    pcm[i] = x;
                }
                var t = engine.Transcribe(pcm, r.Source, lang);
                return new WorkerReply(new WorkerResponse(r.Id, true, Ms: Ms(sw), Transcript: t), []);
            }

            case "bench":
                return new WorkerReply(new WorkerResponse(r.Id, true, Ms: Ms(sw), Bench: engine.Bench()), []);

            case "shutdown":
                return new WorkerReply(new WorkerResponse(r.Id, true), [], Shutdown: true);

            default:
                throw new BadRequestException("op");
        }
    }

    internal static ImageInput ValidateImage(WorkerRequest r, byte[] payload)
    {
        if (r.Format is null || Array.IndexOf(ImageFormats, r.Format) < 0) throw new BadRequestException("format");
        if (payload.Length == 0) throw new BadRequestException("empty image");
        if (r.Format == "bgra8")
        {
            if (r.Width is not int w || r.Height is not int h || r.Stride is not int stride) throw new BadRequestException("bgra8 shape");
            if (w <= 0 || h <= 0 || w > MaxImageSide || h > MaxImageSide) throw new BadRequestException("bgra8 size");
            if (stride < w * 4) throw new BadRequestException("stride");
            long need = (long)stride * (h - 1) + (long)w * 4;
            if (payload.Length < need) throw new BadRequestException("bgra8 payload");
            return new ImageInput("bgra8", w, h, stride, payload);
        }
        // Encoded formats carry their own size; width/height are informational.
        return new ImageInput(r.Format, r.Width ?? 0, r.Height ?? 0, 0, payload);
    }

    /// <summary>"auto", or a Whisper language code: 2 or 3 lower-case letters, or "haw"/"yue"-style codes.</summary>
    internal static bool IsLanguage(string lang)
        => lang == "auto" || (lang.Length is >= 2 and <= 3 && lang.All(c => c is >= 'a' and <= 'z'));

    private static float[] Finish(float[] full, int dim)
    {
        if (VectorMath.HasNonFinite(full)) throw new DeviceFailedException("model output contains NaN");
        if (full.Length < dim) throw new DeviceFailedException($"model dimension {full.Length} below index.dim {dim}");
        return VectorMath.Matryoshka(full, dim);
    }

    private static double Ms(Stopwatch sw) => Math.Round(sw.Elapsed.TotalMilliseconds, 2);

    private static WorkerReply Fail(long id, string error, string why)
    {
        Log.Info($"request rejected: {why}");
        return new WorkerReply(new WorkerResponse(id, false, error), []);
    }

    /// <summary>Best effort id from a header that failed to deserialize as a whole.</summary>
    internal static long TryReadId(ReadOnlySpan<byte> header)
    {
        try
        {
            var reader = new Utf8JsonReader(header);
            if (!reader.Read() || reader.TokenType != JsonTokenType.StartObject) return 0;
            while (reader.Read() && reader.TokenType == JsonTokenType.PropertyName)
            {
                bool isId = reader.ValueTextEquals("id"u8);
                if (!reader.Read()) return 0;
                if (isId && reader.TokenType == JsonTokenType.Number && reader.TryGetInt64(out var id)) return id;
                reader.Skip();
            }
        }
        catch (JsonException) { }
        return 0;
    }
}
