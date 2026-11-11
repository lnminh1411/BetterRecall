// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Collections.Concurrent;
using BetterRecall.Agent.Capture;
using BetterRecall.Agent.Guardrails;
using BetterRecall.Agent.Native;
using BetterRecall.Agent.Privacy;
using BetterRecall.Agent.Worker;
using BetterRecall.Contracts;
using SkiaSharp;
using PBox = BetterRecall.Agent.Privacy.Box;

namespace BetterRecall.Agent.Tests;

internal static class Ocr
{
    /// <summary>OCR line with 10 px per character starting at x.</summary>
    public static OcrLine Line(string text, int x, int y, int h = 20, OcrEntity[]? entities = null)
        => new(text, x, y, text.Length * 10, h, 0.95f, Enumerable.Range(0, text.Length + 1).Select(i => x + i * 10).ToArray(), entities);
}

public class RedactorTests
{
    private static readonly RedactionOptions All = new(true, true, true, true, true, true, true, true);
    private static readonly RedactionOptions Defaults = RedactionOptions.From(new PrivacySettings());

    private static RedactionResult Run(RedactionOptions o, params OcrLine[] lines) => Redactor.Apply(lines, [], o);

    [Fact]
    public void Defaults_are_the_sensitive_detectors_only()
    {
        Assert.True(Defaults.Passwords && Defaults.Cards && Defaults.Ids && Defaults.BirthDates);
        Assert.False(Defaults.Emails || Defaults.Phones || Defaults.Names || Defaults.Addresses);
        Assert.True(new OcrSettings().Enabled);
    }

    [Theory]
    [InlineData("Card 4111 1111 1111 1111 exp 12/29", "Card exp 12/29")]
    [InlineData("Visa 4111-1111-1111-1111", "Visa")]
    [InlineData("Order 4111 1111 1111 1112", "Order 4111 1111 1111 1112")]          // Luhn fails
    [InlineData("Tracking 1234567890123456", "Tracking 1234567890123456")]           // starts with 1
    [InlineData("IBAN DE89 3704 0044 0532 0130 00 BIC COBADEFFXXX", "IBAN BIC COBADEFFXXX")]
    [InlineData("GB82 WEST 1234 5698 7654 32", "")]
    [InlineData("Ref DE89 3704 0044 0532 0130 01", "Ref DE89 3704 0044 0532 0130 01")] // mod 97 fails
    [InlineData("SSN 078-05-1120 on file", "SSN on file")]
    [InlineData("Số CCCD: 001099012345", "Số CCCD:")]
    [InlineData("Passport No. C01X00T47 issued", "Passport No. issued")]
    [InlineData("Password: hunter2", "Password:")]
    [InlineData("Mật khẩu: abc123!", "Mật khẩu:")]
    [InlineData("export OPENAI=sk-proj-abcdefghijklmnopqrstuvwx", "export OPENAI=")]
    [InlineData("Date of birth: 12/03/1990", "Date of birth:")]
    [InlineData("Ngày sinh 12 tháng 3 năm 1990", "Ngày sinh")]
    [InlineData("生年月日 1990年3月12日", "生年月日")]
    [InlineData("Meeting on 12/03/2026", "Meeting on 12/03/2026")]                 // date without birth keyword
    [InlineData("Pin to taskbar", "Pin to taskbar")]
    [InlineData("Forgot password?", "Forgot password?")]
    public void Default_detectors(string line, string kept)
        => Assert.Equal(kept, Run(Defaults, Ocr.Line(line, 0, 0)).Text);

    [Theory]
    [InlineData("Mail anna.schmidt@example.de now", "Mail now")]
    [InlineData("Call +49 30 1234567 today", "Call today")]
    [InlineData("(555) 123-4567", "")]
    [InlineData("Điện thoại 0912 345 678", "Điện thoại")]
    [InlineData("Build 10.0.26100.4652", "Build 10.0.26100.4652")]
    [InlineData("Updated 2026-11-12 10:30", "Updated 2026-11-12 10:30")]
    public void Email_and_phone_detectors(string line, string kept)
        => Assert.Equal(kept, Run(All, Ocr.Line(line, 0, 0)).Text);

    [Fact]
    public void Label_value_in_the_next_field_to_the_right_or_below()
    {
        var r = Run(Defaults,
            Ocr.Line("Password", 10, 10), Ocr.Line("••••••••", 120, 12), Ocr.Line("Sign in", 10, 80),
            Ocr.Line("Date of birth", 300, 10), Ocr.Line("03/12/1991", 300, 35));
        Assert.Equal("Password\nDate of birth\nSign in", r.Text);
        Assert.Equal(2, r.Spans);
        Assert.Contains(r.Boxes, b => b.X <= 120 && b.Right >= 200 && b.Y <= 12);
        Assert.Contains(r.Boxes, b => b.X <= 300 && b.Y <= 35 && b.Bottom >= 55);
    }

    [Fact]
    public void Boxes_follow_character_cuts_with_padding()
    {
        var r = Run(Defaults, Ocr.Line("Password: hunter2", 100, 50));
        var b = Assert.Single(r.Boxes);
        Assert.Equal(new PBox(200 - Redactor.Pad, 50 - Redactor.Pad, 70 + 2 * Redactor.Pad, 20 + 2 * Redactor.Pad), b);
    }

    [Fact]
    public void Entities_redact_names_and_addresses_by_setting()
    {
        var line = Ocr.Line("Ship to John Carter, 742 Evergreen Terrace", 0, 0, entities:
            [new OcrEntity(7, 12, "person", 0.9f), new OcrEntity(21, 21, "address", 0.9f)]);
        Assert.Equal("Ship to , 742 Evergreen Terrace", Run(Defaults with { Names = true }, line).Text);
        Assert.Equal("Ship to John Carter,", Run(Defaults with { Addresses = true }, line).Text);
        Assert.Equal(line.Text, Run(Defaults, line).Text);
    }

    [Fact]
    public void Password_fields_black_out_and_drop_the_characters_inside()
    {
        var field = new PBox(118, 0, 62, 24);   // over the six dots (x 120-180)
        var r = Redactor.Apply([Ocr.Line("Login: anna ●●●●●● done", 0, 2)], [field], Defaults);
        Assert.Contains(field, r.Boxes);
        Assert.Equal("Login: anna done", r.Text);
        // Off: neither the box nor the text change.
        var off = Redactor.Apply([Ocr.Line("Login: anna ●●●●●● done", 0, 2)], [field], Defaults with { Passwords = false });
        Assert.Empty(off.Boxes);
    }

    [Fact]
    public void Title_gets_the_string_detectors()
    {
        var r = Redactor.Apply([], [], All, "Inbox - anna@example.com - Gmail");
        Assert.Equal("Inbox - - Gmail", r.Title);
    }

    [Fact]
    public void Reading_order_groups_rows_then_left_to_right()
    {
        var lines = new[] { Ocr.Line("right", 300, 11), Ocr.Line("second row", 0, 40), Ocr.Line("left", 0, 10) };
        Assert.Equal(["left", "right", "second row"], Redactor.ReadingOrder(lines).Select(l => l.Text));
        Assert.Equal("left\nright\nsecond row", Run(RedactionOptions.None, lines).Text);
    }

    [Fact]
    public void Checksums()
    {
        Assert.True(Redactor.Luhn("4111111111111111"));
        Assert.False(Redactor.Luhn("4111111111111112"));
        Assert.Equal(18, Redactor.IbanSpan("NL91ABNA0417164300"));
        Assert.Equal(22, Redactor.IbanSpan("NL91 ABNA 0417 1643 00 BIC"));
        Assert.Null(Redactor.IbanSpan("NL91ABNA0417164301"));
        Assert.Null(Redactor.IbanSpan("ZZ91ABNA0417164300"));
        Assert.Null(Redactor.CardSpan("4111 1111 1111 1111 2"));
    }
}

public class RedactionMasksTests
{
    [Fact]
    public void Keyframe_boxes_map_to_desktop_and_back()
    {
        var mon = new RECT(1920, 0, 1920 + 2560, 1440);
        var d = RedactionMasks.ToDesktop([new PBox(100, 50, 20, 10)], 1280, 720, mon);
        var r = Assert.Single(d);
        Assert.Equal((1920 + 200, 100, 1920 + 240, 120), (r.Left, r.Top, r.Right, r.Bottom));
        var k = Assert.Single(RedactionMasks.ToKeyframe(d, mon, 1280, 720));
        Assert.Equal(new PBox(100, 50, 20, 10), k);
        // Off-monitor rectangles are dropped, partial ones clipped.
        Assert.Empty(RedactionMasks.ToKeyframe([new RECT(0, 0, 100, 100)], mon, 1280, 720));
        Assert.Equal(new PBox(0, 0, 5, 5), Assert.Single(RedactionMasks.ToKeyframe([new RECT(1900, -10, 1930, 10)], mon, 1280, 720)));
    }

    [Fact]
    public void Masks_replace_per_monitor_and_merge_for_the_replay()
    {
        var m = new RedactionMasks();
        m.Set(0, [new RECT(0, 0, 10, 10)]);
        m.Set(1, [new RECT(100, 0, 110, 10), new RECT(120, 0, 130, 10)]);
        Assert.Equal(3, m.All().Length);
        m.Set(1, []);
        Assert.Single(m.All());
        var snap = GuardSnapshot.Closed with { MaskRects = [new RECT(5, 5, 6, 6)], Secrets = [new RECT(7, 7, 8, 8)] };
        Assert.Equal(3, CaptureService.MergeMasks(snap, true, m.All()).Length);
        Assert.Equal(2, CaptureService.MergeMasks(snap, false, m.All()).Length);
        m.Clear();
        Assert.Same(snap.MaskRects, CaptureService.MergeMasks(snap, false, m.All()));
    }

    [Fact]
    public unsafe void Fill_blacks_out_clipped_boxes()
    {
        var px = new byte[8 * 4 * 4];
        Array.Fill(px, (byte)255);
        fixed (byte* p = px) RedactionMasks.Fill(p, 8, 4, 32, [new PBox(6, 2, 10, 10)]);
        Assert.Equal(new byte[] { 0, 0, 0, 255 }, px.AsSpan((2 * 8 + 6) * 4, 4).ToArray());
        Assert.Equal(new byte[] { 0, 0, 0, 255 }, px.AsSpan((3 * 8 + 7) * 4, 4).ToArray());
        Assert.Equal(255, px[(2 * 8 + 5) * 4]);
        Assert.Equal(255, px[(1 * 8 + 6) * 4]);
    }
}

public class KeyframeRedactionTests
{
    private sealed class FakeReader(Func<OcrResult> answer) : IKeyframeReader
    {
        public int Calls;
        public bool? Entities;
        public Task<OcrResult> ReadAsync(ReadOnlyMemory<byte> bgra, int width, int height, int stride, bool entities, CancellationToken ct)
        {
            Calls++;
            Entities = entities;
            return Task.FromResult(answer());
        }
    }

    private static unsafe KeyframeGrab WhiteGrab(int w, int h, RECT[]? secrets, ulong seed = 0)
    {
        var req = new KeyframeRequest(DateTimeOffset.UtcNow, "app", @"C:\app.exe", "Title", "ctx" + seed, true, w, 0, "", secrets, new RECT(0, 0, w * 2, h * 2));
        var g = KeyframeGrab.Alloc(0, req, w, h, 0);
        new Span<byte>(g.Pixels, w * h * 4).Fill(255);
        return g;
    }

    private static async Task<(FakeStore Store, KeyframePipeline Pipe)> Run(Settings s, IKeyframeReader reader, RedactionMasks masks, KeyframeGrab grab, int expectStored)
    {
        var store = new FakeStore();
        var stored = new ConcurrentQueue<CaptureMeta>();
        var pipe = new KeyframePipeline(() => store, () => s, stored.Enqueue, reader, masks);
        pipe.Post(grab);
        for (int i = 0; i < 200 && pipe.Kept + pipe.RedactDropped + pipe.Failed == 0; i++) await Task.Delay(20);
        Assert.Equal(expectStored, store.Screens.Count);
        return (store, pipe);
    }

    private static SKColor Pixel(byte[] webp, int x, int y)
    {
        using var bmp = SKBitmap.Decode(webp);
        return bmp.GetPixel(x, y);
    }

    [Fact]
    public async Task Stored_keyframe_has_black_boxes_and_redacted_text()
    {
        var s = new Settings();
        s.Keyframes.Quality = 95;
        var reader = new FakeReader(() => new OcrResult([
            Ocr.Line("Card: 4111 1111 1111 1111", 10, 10),
            Ocr.Line("Password:", 10, 50), Ocr.Line("hunter2", 150, 50),
            Ocr.Line("Meeting notes", 10, 100),
        ], "onnx", 5));
        var masks = new RedactionMasks();
        // Password field at desktop (600,300)-(700,340) = keyframe (300,150)-(350,170).
        var (store, pipe) = await Run(s, reader, masks, WhiteGrab(400, 200, [new RECT(600, 300, 700, 340)]), 1);
        await pipe.DisposeAsync();
        var cap = store.Screens.Values.Single();
        Assert.Equal("Card:\nPassword:\nMeeting notes", cap.Text);
        Assert.False(reader.Entities);
        Assert.True(Pixel(cap.Image, 100, 20).Red < 40);    // card digits
        Assert.True(Pixel(cap.Image, 170, 60).Red < 40);    // hunter2
        Assert.True(Pixel(cap.Image, 320, 160).Red < 40);   // password field
        Assert.True(Pixel(cap.Image, 40, 110).Red > 200);   // meeting notes kept
        Assert.True(Pixel(cap.Image, 380, 10).Red > 200);
        // Replay masks: the same boxes in desktop pixels (2x).
        Assert.Contains(masks.All(), r => r.Left <= 600 && r.Right >= 700 && r.Top <= 300 && r.Bottom >= 340);
        Assert.Equal(3, masks.All().Length);   // field, card digits, hunter2
    }

    [Fact]
    public async Task Ocr_failure_drops_the_keyframe_when_text_redaction_is_on()
    {
        var reader = new FakeReader(() => throw new WorkerException("Worker pipe closed."));
        var (_, pipe) = await Run(new Settings(), reader, new RedactionMasks(), WhiteGrab(64, 32, null), 0);
        Assert.Equal(1, pipe.RedactDropped);
        await pipe.DisposeAsync();
    }

    [Fact]
    public async Task Ocr_failure_stores_without_text_when_no_text_detector_is_on()
    {
        var s = new Settings();
        s.Privacy.RedactPasswords = s.Privacy.RedactCards = s.Privacy.RedactIds = s.Privacy.RedactBirthDates = false;
        var reader = new FakeReader(() => throw new WorkerException("Device failed: busy."));
        var (store, pipe) = await Run(s, reader, new RedactionMasks(), WhiteGrab(64, 32, null), 1);
        Assert.Null(store.Screens.Values.Single().Text);
        Assert.Equal(1, pipe.OcrFailed);
        await pipe.DisposeAsync();
    }

    [Fact]
    public async Task Missing_ocr_model_stores_without_text()
    {
        var reader = new FakeReader(() => throw new WorkerException("Model not installed: ocr."));
        var (store, pipe) = await Run(new Settings(), reader, new RedactionMasks(), WhiteGrab(64, 32, null), 1);
        Assert.Null(store.Screens.Values.Single().Text);
        Assert.Equal(1, pipe.OcrUnavailable);
        await pipe.DisposeAsync();
    }

    [Fact]
    public async Task Ocr_off_skips_the_worker_and_entities_follow_the_name_setting()
    {
        var s = new Settings();
        s.Ocr.Enabled = false;
        var reader = new FakeReader(() => new OcrResult([], "onnx", 0));
        var (store, pipe) = await Run(s, reader, new RedactionMasks(), WhiteGrab(64, 32, null), 1);
        Assert.Equal(0, reader.Calls);
        Assert.Null(store.Screens.Values.Single().Text);
        await pipe.DisposeAsync();

        var s2 = new Settings();
        s2.Privacy.RedactNames = true;
        var (store2, pipe2) = await Run(s2, reader, new RedactionMasks(), WhiteGrab(64, 32, null, 1), 1);
        Assert.True(reader.Entities);
        Assert.Equal("", store2.Screens.Values.Single().Text);
        await pipe2.DisposeAsync();
    }
}
