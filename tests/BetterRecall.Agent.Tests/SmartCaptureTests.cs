// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Diagnostics;
using BetterRecall.Agent.Capture;
using BetterRecall.Agent.Host;
using BetterRecall.Agent.Thumbs;
using BetterRecall.Contracts;
using SkiaSharp;
using Xunit.Abstractions;

namespace BetterRecall.Agent.Tests;

/// <summary>Synthetic screens: flat background, "text" (dark 1-2 px strokes) and photo-like gradients.</summary>
internal static class Screens
{
    public static byte[] Flat(int w, int h, byte v = 240)
    {
        var px = new byte[w * h * 4];
        for (int i = 0; i < px.Length; i += 4) { px[i] = px[i + 1] = px[i + 2] = v; px[i + 3] = 255; }
        return px;
    }

    public static void Rect(byte[] px, int w, int x0, int y0, int rw, int rh, byte b, byte g, byte r)
    {
        for (int y = y0; y < y0 + rh; y++)
            for (int x = x0; x < x0 + rw; x++)
            {
                int i = (y * w + x) * 4;
                px[i] = b; px[i + 1] = g; px[i + 2] = r;
            }
    }

    /// <summary>Glyph-like strokes: vertical bars 1-2 px wide every 4-6 px on text lines 10 px high, 6 px apart.</summary>
    public static void Text(byte[] px, int w, int x0, int y0, int rw, int rh, int seed)
    {
        var rnd = new Random(seed);
        for (int line = y0; line + 10 <= y0 + rh; line += 16)
            for (int x = x0; x < x0 + rw - 2; x += rnd.Next(4, 7))
            {
                int width = rnd.Next(1, 3), top = line + rnd.Next(0, 3);
                for (int y = top; y < line + 10; y++)
                    for (int dx = 0; dx < width; dx++)
                    {
                        int i = (y * w + x + dx) * 4;
                        px[i] = px[i + 1] = px[i + 2] = 20;
                    }
            }
    }

    public static void Gradient(byte[] px, int w, int x0, int y0, int rw, int rh)
    {
        for (int y = y0; y < y0 + rh; y++)
            for (int x = x0; x < x0 + rw; x++)
            {
                int i = (y * w + x) * 4;
                px[i] = (byte)(x * 255 / w); px[i + 1] = (byte)(y * 255 / (y0 + rh)); px[i + 2] = 128;
            }
    }

    public static ChangeMap Map(byte[] px, int w, int h) => ChangeMap.Compute(px, w, h, w * 4);
}

public class ChangeMapTests
{
    private const int W = 1280, H = 720;

    [Fact]
    public void Identical_frames_score_zero_and_size_change_is_first()
    {
        var a = Screens.Flat(W, H);
        Screens.Text(a, W, 64, 64, 600, 300, 1);
        var r = ChangeMap.Compare(Screens.Map(a, W, H), Screens.Map(a, W, H));
        Assert.True(r.Comparable);
        Assert.Equal(0, r.Score);
        Assert.Equal(ChangeResult.First, ChangeMap.Compare(null, Screens.Map(a, W, H)));
        var small = Screens.Flat(640, 360);
        Assert.False(ChangeMap.Compare(Screens.Map(small, 640, 360), Screens.Map(a, W, H)).Comparable);
    }

    [Fact]
    public void Flat_area_change_scores_its_share_of_the_screen()
    {
        var a = Screens.Flat(W, H);
        var b = (byte[])a.Clone();
        Screens.Rect(b, W, 0, 0, 640, 288, 30, 90, 200); // a quarter-ish block in another colour: 20 % of the screen
        var r = ChangeMap.Compare(Screens.Map(a, W, H), Screens.Map(b, W, H));
        Assert.InRange(r.ChangedPercent, 19.5, 20.5);
        Assert.Equal(0, r.TextPercent);
        Assert.InRange(r.Score, 19.5, 20.5);
    }

    [Fact]
    public void Text_like_change_counts_five_times()
    {
        var a = Screens.Flat(W, H);
        var b = (byte[])a.Clone();
        Screens.Text(b, W, 0, 0, 320, 96, 3); // 10 x 3 tiles = 3.3 % of the screen
        var mb = Screens.Map(b, W, H);
        Assert.True(mb.Text[0]);
        var r = ChangeMap.Compare(Screens.Map(a, W, H), mb);
        Assert.InRange(r.TextPercent, 3.0, 3.5);
        Assert.InRange(r.Score, 15, 17.5);   // passes the default 15 %
    }

    [Fact]
    public void Flat_and_gradient_tiles_are_not_text()
    {
        var a = Screens.Flat(W, H);
        Screens.Gradient(a, W, 0, 0, 640, 360);
        var m = Screens.Map(a, W, H);
        Assert.DoesNotContain(true, m.Text);
    }

    [Fact]
    public void Clear_zeroes_the_map()
    {
        var a = Screens.Flat(W, H);
        var m = Screens.Map(a, W, H);
        m.Clear();
        Assert.All(m.Means, b => Assert.Equal(0, b));
    }

    [Theory]
    // force, trigger, context, comparable, score, hash, threshold, dirtyMin, expected
    [InlineData(true, false, false, true, 0, 0, 6, 15, "Force")]
    [InlineData(false, false, false, false, 100, 64, 6, 15, "First")]
    [InlineData(false, false, true, true, 0, 0, 6, 15, "Switch")]
    [InlineData(false, false, false, true, 14.9, 2, 6, 15, "Skip")]
    [InlineData(false, false, false, true, 15, 2, 6, 15, "Change")]
    [InlineData(false, false, false, true, 3, 7, 6, 15, "Hash")]
    [InlineData(false, false, false, true, 3, 7, 0, 15, "Skip")]      // dhashThreshold 0 turns the hash test off
    [InlineData(false, true, false, true, 0.6, 0, 6, 15, "Trigger")]  // event trigger: any visible change
    [InlineData(false, true, false, true, 0.2, 1, 6, 15, "Skip")]     // caret blink after a mouse stop
    [InlineData(false, false, false, true, 0, 0, 6, 0, "Skip")]       // identical frame never kept on the timer
    [InlineData(false, false, false, true, 0.1, 0, 6, 0, "Change")]
    public void Policy_table(bool force, bool trigger, bool context, bool comparable, double score, int hash, int threshold, double dirtyMin, string expected)
    {
        var change = new ChangeResult(score, 0, score, comparable);
        Assert.Equal(Enum.Parse<KeepReason>(expected), CapturePolicy.Decide(force, trigger, context, change, hash, threshold, dirtyMin));
    }

    [Fact]
    public void Dedup_filter_compares_with_the_last_stored_keyframe()
    {
        var d = new DedupFilter();
        var a = Screens.Flat(W, H);
        var req = new KeyframeRequest(DateTimeOffset.UtcNow, "app", "", "title", "app\u0001title", false, W, 0.5);
        var (r1, c1) = d.Decide(Screens.Map(a, W, H), 1, req, 6, 15);
        Assert.Equal(KeepReason.First, r1);
        Assert.False(c1.Comparable);
        d.Accept(1, req.Context, Screens.Map(a, W, H));

        // Small changes accumulate against the stored frame until they pass 15 %.
        var b = (byte[])a.Clone();
        Screens.Rect(b, W, 0, 0, 1280, 64, 0, 0, 0); // 2 rows of tiles = 8.9 %
        Assert.Equal(KeepReason.Skip, d.Decide(Screens.Map(b, W, H), 1, req, 6, 15).Reason);
        Screens.Rect(b, W, 0, 64, 1280, 64, 0, 0, 0); // 17.8 % against the stored frame
        Assert.Equal(KeepReason.Change, d.Decide(Screens.Map(b, W, H), 1, req, 6, 15).Reason);

        // Window switch keeps whatever the pixels do.
        Assert.Equal(KeepReason.Switch, d.Decide(Screens.Map(a, W, H), 1, req with { Context = "other\u0001x" }, 6, 15).Reason);
        d.Reset();
        Assert.Null(d.LastHash);
        Assert.Equal(KeepReason.First, d.Decide(Screens.Map(a, W, H), 1, req, 6, 15).Reason);
    }
}

public class ImageCodecTests(ITestOutputHelper output)
{
    private static byte[] Ui(int w, int h)
    {
        var px = Screens.Flat(w, h, 250);
        Screens.Rect(px, w, 0, 0, w, 40, 60, 40, 30);
        Screens.Text(px, w, 40, 80, w - 80, h - 200, 5);
        Screens.Gradient(px, w, w / 2, h - 110, w / 2 - 10, 100);
        return px;
    }

    private static double MeanAbsDiff(byte[] a, byte[] b)
    {
        long sum = 0;
        int n = 0;
        for (int i = 0; i < a.Length; i += 4)
            for (int c = 0; c < 3; c++, n++) sum += Math.Abs(a[i + c] - b[i + c]);
        return sum / (double)n;
    }

    [Theory]
    [InlineData("avif", "image/avif")]
    [InlineData("jxl", "image/jxl")]
    [InlineData("webp", "image/webp")]
    [InlineData("jpeg", "image/jpeg")]
    public unsafe void Round_trip_keeps_size_and_pixels_close(string format, string mime)
    {
        Assert.Equal(format, ImageCodec.Resolve(format)); // the DLLs ship next to the test assembly
        const int w = 640, h = 360;
        var px = Ui(w, h);
        byte[] enc;
        fixed (byte* p = px) enc = ImageCodec.EncodeBgra(p, w, h, w * 4, format, 75);
        Assert.Equal(mime, ImageCodec.Mime(format));
        var (dec, dw, dh) = ImageCodec.DecodeBgra(enc, mime);
        Assert.Equal((w, h), (dw, dh));
        double mad = MeanAbsDiff(px, dec);
        output.WriteLine($"{format}: {enc.Length} bytes, mean abs diff {mad:F2}");
        Assert.True(mad < 12, $"{format} mean abs diff {mad}");
    }

    [Fact]
    public unsafe void Transcode_makes_avif_and_jxl_readable_and_resize_works()
    {
        const int w = 640, h = 360;
        var px = Ui(w, h);
        foreach (var format in new[] { "avif", "jxl" })
        {
            byte[] enc;
            fixed (byte* p = px) enc = ImageCodec.EncodeBgra(p, w, h, w * 4, format, 75);
            var (bytes, mime, tw, th) = ImageCodec.Transcode(enc, ImageCodec.Mime(format), "webp", 90);
            Assert.Equal("image/webp", mime);
            Assert.Equal((w, h), (tw, th));
            Assert.Equal("RIFF", System.Text.Encoding.ASCII.GetString(bytes, 0, 4));
            var (thumb, rw, rh) = ImageCodec.ResizeToWebp(enc, ImageCodec.Mime(format), 320, 70);
            Assert.Equal((320, 180), (rw, rh));
            Assert.NotNull(SKBitmap.Decode(thumb));
        }
        Assert.False(ImageCodec.CommonlyDecodable("image/avif"));
        Assert.True(ImageCodec.CommonlyDecodable("image/webp"));
        Assert.Equal("webp", ImageCodec.Resolve("bogus"));
    }

    [Fact]
    public unsafe void Thumbnailer_transcodes_avif_for_the_ui_and_jpeg_for_export()
    {
        var store = new FakeStore();
        const int w = 640, h = 360;
        var px = Ui(w, h);
        byte[] avif, webp;
        fixed (byte* p = px)
        {
            avif = ImageCodec.EncodeBgra(p, w, h, w * 4, "avif", 75);
            webp = ImageCodec.EncodeBgra(p, w, h, w * 4, "webp", 75);
        }
        var thumbs = new Thumbnailer(() => store, () => new Settings());
        var a = new ThumbData("a", "image/avif", w, h, avif);
        var ui = ((IThumbnailer)thumbs).Transcode(a, "ui");
        Assert.Equal("image/webp", ui.Mime);
        var plain = new ThumbData("b", "image/webp", w, h, webp);
        Assert.Same(plain, ((IThumbnailer)thumbs).Transcode(plain, "ui"));
        var jpeg = ((IThumbnailer)thumbs).Transcode(plain, "jpeg");
        Assert.Equal("image/jpeg", jpeg.Mime);
        Assert.Equal(0xFF, jpeg.Bytes[0]);
    }

    /// <summary>Codec spike on real screenshots: BR_CODEC_SPIKE=folder of PNGs. Prints size and encode/decode ms per format.</summary>
    [Fact]
    public unsafe void Spike_on_real_screenshots()
    {
        var dir = Environment.GetEnvironmentVariable("BR_CODEC_SPIKE");
        if (string.IsNullOrEmpty(dir) || !Directory.Exists(dir)) return;
        if (int.TryParse(Environment.GetEnvironmentVariable("BR_CODEC_SPIKE_SPEED"), out var speed)) ImageCodec.AvifSpeed = speed;
        if (int.TryParse(Environment.GetEnvironmentVariable("BR_CODEC_SPIKE_EFFORT"), out var effort)) ImageCodec.JxlEffort = effort;
        foreach (var file in Directory.GetFiles(dir, "*.png"))
        {
            using var src0 = SKBitmap.Decode(file);
            int w = Math.Min(1280, src0.Width), h = (int)((long)src0.Height * w / src0.Width);
            using var src = src0.Resize(new SKImageInfo(w, h, SKColorType.Bgra8888, SKAlphaType.Opaque), new SKSamplingOptions(SKFilterMode.Linear, SKMipmapMode.Linear));
            var px = new byte[w * h * 4];
            System.Runtime.InteropServices.Marshal.Copy(src.GetPixels(), px, 0, px.Length);
            var qs = (Environment.GetEnvironmentVariable("BR_CODEC_SPIKE_Q") ?? "75").Split(',').Select(int.Parse).ToArray();
            foreach (var (f, q) in new[] { "webp", "jpeg", "avif", "jxl" }.SelectMany(f => qs.Select(q => (f, q))))
            {
                byte[] enc = [];
                var sw = Stopwatch.StartNew();
                for (int i = 0; i < 3; i++) fixed (byte* p = px) enc = ImageCodec.EncodeBgra(p, w, h, w * 4, f, q);
                double encMs = sw.Elapsed.TotalMilliseconds / 3;
                sw.Restart();
                var (dec, _, _) = ImageCodec.DecodeBgra(enc, ImageCodec.Mime(f));
                double decMs = sw.Elapsed.TotalMilliseconds;
                output.WriteLine($"{Path.GetFileName(file)} {w}x{h} {f} q{q}: {enc.Length / 1024.0:F1} KiB, encode {encMs:F0} ms, decode {decMs:F0} ms, mad {MeanAbsDiff(px, dec):F2}");
            }
        }
    }
}

public class StorageGuardTests
{
    private const long Gb = StorageGuard.Gb;

    private static (StorageGuard Guard, FakeStore Store, Settings Settings, long[] Disk) Make(long diskFree, long files, long free, bool overwrite)
    {
        var store = new FakeStore { Use = new StorageUse(files, free) };
        var s = new Settings();
        s.Storage.MinFreeGb = 25;
        s.Storage.MaxGb = 20;
        s.Storage.Overwrite = overwrite;
        var disk = new[] { diskFree };
        return (new StorageGuard(() => store, () => s, () => disk[0]), store, s, disk);
    }

    [Fact]
    public void Low_disk_pauses_once_and_resumes_with_margin()
    {
        var (g, store, _, disk) = Make(24 * Gb, 5 * Gb, 0, overwrite: false);
        var d = g.Check();
        Assert.True(d.Pause);
        Assert.True(d.Notify);
        Assert.Equal("disk", d.Why);
        Assert.False(g.Check().Notify);           // balloon once
        disk[0] = 25 * Gb + Gb / 2;                // above the limit but inside the margin: stays paused
        Assert.True(g.Check().Pause);
        disk[0] = 26 * Gb + 1;
        Assert.False(g.Check().Pause);
        Assert.Equal(0, store.DeleteCalls);
    }

    [Fact]
    public void Reusable_free_pages_count_as_free_space()
    {
        var (g, _, _, _) = Make(24 * Gb, 6 * Gb, 2 * Gb, overwrite: false);
        Assert.False(g.Check().Pause);
    }

    [Fact]
    public void Over_size_pauses_without_overwrite()
    {
        var (g, _, _, _) = Make(500 * Gb, 21 * Gb, 0, overwrite: false);
        var d = g.Check();
        Assert.True(d.Pause);
        Assert.Equal("size", d.Why);
    }

    [Fact]
    public void Overwrite_deletes_until_margin_instead_of_pausing()
    {
        var (g, store, _, _) = Make(500 * Gb, 21 * Gb, 0, overwrite: true);
        store.Deletable = 100_000;
        store.FreedPerRow = 1024 * 1024; // 1 MiB per capture
        var d = g.Check();
        Assert.False(d.Pause);
        Assert.True(d.Deleted > 0);
        Assert.True(store.Use.LiveBytes <= 20 * Gb - StorageGuard.Margin(new StorageSettings()));
    }

    [Fact]
    public void Overwrite_pauses_when_nothing_is_left_to_delete()
    {
        var (g, store, _, _) = Make(10 * Gb, 1 * Gb, 0, overwrite: true);
        store.Deletable = 3;
        store.FreedPerRow = 1024;
        var d = g.Check();
        Assert.True(d.Pause);
        Assert.Equal(3, d.Deleted);
        Assert.Equal("disk", d.Why);
    }

    [Fact]
    public void Storage_pause_stops_keyframes_but_not_the_replay_ring()
    {
        var p = new PauseController();
        p.Set("storage");
        Assert.True(p.KeyframesPaused);
        Assert.False(p.ContinuousPaused(duringFullscreen: true));
        p.Set("user");
        Assert.Equal("storage", p.Current().Reason);
    }
}
