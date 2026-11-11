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
/// Redacted is true when the text was checked (or no text detector is on). OCR off or not installed: the image goes out
/// as stored with Redacted false, so the preview says it was not checked. OCR failing otherwise: describe fails rather
/// than sending an unchecked image.
/// </summary>
internal sealed class DescribeRedactor(IKeyframeReader reader, Func<Settings> settings) : IDescribeRedactor
{
    private const int Quality = 90;

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
        try
        {
            bmp.GetPixelSpan().CopyTo(pixels);
            OcrResult ocr;
            using var timeout = CancellationTokenSource.CreateLinkedTokenSource(ct);
            timeout.CancelAfter(KeyframePipeline.OcrTimeout);
            try { ocr = await reader.ReadAsync(pixels, w, h, stride, red.Entities, timeout.Token).ConfigureAwait(false); }
            catch (OperationCanceledException) when (!ct.IsCancellationRequested)
            {
                throw new DescribeException("Text recognition timed out, so the image was not checked and is not sent. Try again.");
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
            var r = Redactor.Apply(ocr.Lines, [], red);
            if (r.Boxes.Length == 0) return image with { Redacted = true };
            unsafe
            {
                fixed (byte* p = pixels)
                {
                    RedactionMasks.Fill(p, w, h, stride, r.Boxes);
                    return new OutgoingImage(ImageCodec.EncodeBgra(p, w, h, stride, "webp", Quality), ImageCodec.Mime("webp"), w, h, true);
                }
            }
        }
        finally
        {
            Array.Clear(pixels);
            ClearPixels(bmp);
        }
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
