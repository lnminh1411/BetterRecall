// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using BetterRecall.Agent.Capture;
using BetterRecall.Agent.Util;
using BetterRecall.Agent.Worker;
using BetterRecall.Contracts;
using BetterRecall.Core.Ai;
using SkiaSharp;

namespace BetterRecall.Agent.Privacy;

/// <summary>
/// Cloud describe hook (IDescribeRedactor): runs OCR and the privacy.redact* detectors again on the image about to leave
/// the PC, with the current settings, paints black boxes and re-encodes it as WebP. The stored keyframe was already
/// redacted when it was captured; this pass also covers keyframes stored before redaction existed or with other settings.
/// Redacted is true when the text was checked (or no text detector is on); with names or addresses on and the detector
/// model missing or broken it is false, patterns painted all the same. OCR off or not installed: the image goes out
/// as stored with Redacted false, so the preview says it was not checked. OCR failing otherwise: describe fails rather
/// than sending an unchecked image.
/// The outgoing image is often smaller than the capture (a 1280 px preview of an AVIF or JPEG XL keyframe, a 768 px chat
/// image): OCR then reads the stored keyframe at capture size and the boxes are scaled onto the outgoing image, so small
/// text is found as it was at capture (CA-S1). OCR waits as long as keyframe OCR does (KeyframePipeline.OcrWait, at
/// least OcrTimeout), and only after the worker is up; a timeout behind other worker work says so (A-M6).
/// </summary>
internal sealed class DescribeRedactor(IKeyframeReader reader, Func<Settings> settings, Func<IStore?>? store = null, Func<TimeSpan>? ocrWait = null)
    : IDescribeRedactor
{
    private const int Quality = 90;
    internal const string TimedOut = "Text recognition timed out, so the image was not checked and is not sent. Try again.";
    internal const string Busy = "Text recognition is busy with other work, so the image was not checked and is not sent. Try again in a minute.";
    internal const string Starting = "Text recognition is starting, so the image was not checked and is not sent. Try again in a minute.";

    /// <summary>Shortest OCR wait (KeyframePipeline.OcrTimeout; tests shorten it).</summary>
    internal TimeSpan MinWait { get; init; } = KeyframePipeline.OcrTimeout;

    /// <summary>Longest wait for the worker to start before the read (a first start benches devices for minutes): a
    /// describe or a chat image does not hang behind it (review S9).</summary>
    internal TimeSpan StartWait { get; init; } = TimeSpan.FromSeconds(60);

    /// <summary>Cloud chat may send images this redactor returned with Redacted set (it really checked them).</summary>
    public bool RedactsImages => true;

    public async Task<OutgoingImage> RedactAsync(string captureId, OutgoingImage image, CancellationToken ct)
    {
        var s = settings();
        var red = RedactionOptions.From(s.Privacy);
        if (!red.AnyText) return image with { Redacted = true };
        if (!s.Ocr.Enabled) return image with { Redacted = false };

        using var bmp = Decode(image.Bytes);
        int w = bmp.Width, h = bmp.Height, stride = bmp.RowBytes;
        var pixels = new byte[stride * h];
        byte[]? full = null;
        try
        {
            bmp.GetPixelSpan().CopyTo(pixels);
            (full, int fw, int fh, bool fullRead) = FullSize(captureId, w, h);
            OcrResult ocr;
            var wait = ocrWait?.Invoke() is { } aw && aw > MinWait ? aw : MinWait;
            int ahead = 0;
            try
            {
                // The worker starts first, outside the wait (TH2); requests it still owes say whether a timeout is "busy".
                try { ahead = await reader.ReadyAsync(ct).WaitAsync(StartWait, ct).ConfigureAwait(false); }
                catch (TimeoutException) { throw new DescribeException(Starting); }
                using var timeout = CancellationTokenSource.CreateLinkedTokenSource(ct);
                timeout.CancelAfter(wait);
                ocr = full is null
                    ? await reader.ReadAsync(pixels, w, h, stride, red.Entities, timeout.Token).ConfigureAwait(false)
                    : await reader.ReadAsync(full, fw, fh, fw * 4, red.Entities, timeout.Token).ConfigureAwait(false);
            }
            catch (OperationCanceledException) when (!ct.IsCancellationRequested)
            {
                Log.Info($"describe redaction: ocr took over {wait.TotalSeconds:0} s ({ahead} worker requests ahead of it)");
                throw new DescribeException(ahead > 0 ? Busy : TimedOut);
            }
            catch (WorkerException ex) when (ex.Message.StartsWith("Model not installed", StringComparison.Ordinal))
            {
                return image with { Redacted = false };
            }
            catch (WorkerException ex)
            {
                Log.Warn($"describe redaction: ocr failed ({ex.Message})");
                throw new DescribeException("Text recognition failed, so the image was not checked and is not sent. Try again.");
            }
            // Names or addresses asked for but the detector model is missing or broken: the patterns are still painted,
            // yet the image was not fully checked, so it is not Redacted (chat sends text only, the preview says so) (N1).
            bool checkedAll = !(red.Entities && ocr.EntitiesMissing) && fullRead;
            if (!checkedAll) Log.Warn("describe redaction: " + (fullRead ? "name and address detector unavailable" : "stored keyframe unreadable, small text unchecked") + ", image marked not checked");
            var r = Redactor.Apply(ocr.Lines, [], red);
            if (r.Boxes.Length == 0) return image with { Redacted = checkedAll };
            var boxes = full is null ? r.Boxes : ScaleBoxes(r.Boxes, fw, fh, w, h);
            unsafe
            {
                fixed (byte* p = pixels)
                {
                    RedactionMasks.Fill(p, w, h, stride, boxes);
                    return new OutgoingImage(ImageCodec.EncodeBgra(p, w, h, stride, "webp", Quality), ImageCodec.Mime("webp"), w, h, checkedAll);
                }
            }
        }
        finally
        {
            Array.Clear(pixels);
            if (full is not null) Array.Clear(full);
            ClearPixels(bmp);
        }
    }

    /// <summary>The stored keyframe as BGRA when it is larger than the outgoing image (w x h), else null (OCR the outgoing
    /// image). Read false: the keyframe exists but could not be decoded, so the smaller image is all that gets checked.</summary>
    private (byte[]? Bgra, int W, int H, bool Read) FullSize(string captureId, int w, int h)
    {
        ThumbData? kf;
        try { kf = store?.Invoke()?.GetKeyframe(captureId); }
        catch (Exception ex) { Log.Debug($"describe redaction: keyframe read: {ex.Message}"); return (null, 0, 0, false); }
        if (kf is null) return (null, 0, 0, true);
        try
        {
            if (kf.Width <= w && kf.Height <= h) return (null, 0, 0, true);
            var (bgra, fw, fh) = ImageCodec.DecodeBgra(kf.Bytes, kf.Mime);
            return (bgra, fw, fh, true);
        }
        catch (Exception ex)
        {
            Log.Warn($"describe redaction: keyframe decode ({kf.Mime})", ex);
            return (null, 0, 0, false);
        }
        finally { Array.Clear(kf.Bytes); }
    }

    /// <summary>Boxes on a fw x fh image onto a w x h copy of it, rounded outwards and grown by the resize filter's
    /// reach (ceil(1 / scale) px, at least 1: glyph ink spreads past the scaled box in a downscaled, lossy copy, S12),
    /// clipped to the image. Pure.</summary>
    internal static Box[] ScaleBoxes(Box[] boxes, int fw, int fh, int w, int h)
    {
        double sx = (double)w / fw, sy = (double)h / fh;
        int gx = Math.Max(1, (int)Math.Ceiling(1 / sx)), gy = Math.Max(1, (int)Math.Ceiling(1 / sy));
        var outp = new List<Box>(boxes.Length);
        foreach (var b in boxes)
        {
            int x0 = Math.Max(0, (int)Math.Floor(b.X * sx) - gx), y0 = Math.Max(0, (int)Math.Floor(b.Y * sy) - gy);
            int x1 = Math.Min(w, (int)Math.Ceiling(b.Right * sx) + gx), y1 = Math.Min(h, (int)Math.Ceiling(b.Bottom * sy) + gy);
            if (x1 > x0 && y1 > y0) outp.Add(new Box(x0, y0, x1 - x0, y1 - y0));
        }
        return outp.ToArray();
    }

    private static SKBitmap Decode(byte[] bytes)
    {
        using var src = SKBitmap.Decode(bytes) ?? throw new DescribeException("The image could not be read.");
        if (src.ColorType == SKColorType.Bgra8888) return src.Copy();
        var bgra = new SKBitmap(new SKImageInfo(src.Width, src.Height, SKColorType.Bgra8888, SKAlphaType.Premul));
        if (!src.CopyTo(bgra, SKColorType.Bgra8888)) { bgra.Dispose(); throw new DescribeException("The image could not be read."); }
        ClearPixels(src);
        return bgra;
    }

    private static unsafe void ClearPixels(SKBitmap b)
    {
        if (b.GetPixels() != 0) new Span<byte>((void*)b.GetPixels(), b.ByteCount).Clear();
    }
}
