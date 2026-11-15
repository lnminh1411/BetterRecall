// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using BetterRecall.Worker.Models.Ocr;

namespace BetterRecall.Worker.Tests;

/// <summary>Bug sweep 2, C12: the whole detector map and every recogniser row are checked for NaN and infinity.</summary>
public class BatchHOcrTests
{
    [Theory]
    [InlineData(0)]
    [InlineData(4095)]
    [InlineData(4096)]          // the old check stopped here
    [InlineData(1920 * 1088 - 1)]
    public void C12_Non_finite_value_anywhere_in_a_detector_map_is_found(int at)
    {
        var map = new float[1920 * 1088];
        Assert.True(PaddleOcr.AllFinite(map));
        map[at] = float.NaN;
        Assert.False(PaddleOcr.AllFinite(map));
        map[at] = float.PositiveInfinity;
        Assert.False(PaddleOcr.AllFinite(map));
    }

    [Fact]
    public void C12_Nan_recogniser_row_is_not_read_as_blank()
    {
        // Ctc alone would turn a NaN row into blank (class 0): characters vanish while Confidence stays finite.
        const int steps = 3, classes = 3;
        var probs = new float[steps * classes];
        probs[0 * classes + 1] = 0.9f;
        for (int k = 0; k < classes; k++) probs[1 * classes + k] = float.NaN;
        probs[2 * classes + 2] = 0.9f;
        var ctc = OcrMath.Ctc(probs, steps, classes, ["", "a", "b"]);
        Assert.Equal("ab", ctc.Text);
        Assert.True(float.IsFinite(ctc.Confidence));
        Assert.False(PaddleOcr.AllFinite(probs));   // so Recognise now fails the frame instead
    }
}
