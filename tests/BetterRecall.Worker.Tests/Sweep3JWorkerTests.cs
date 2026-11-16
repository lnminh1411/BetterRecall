// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using BetterRecall.Worker.Models.Ner;
using BetterRecall.Worker.Protocol;

namespace BetterRecall.Worker.Tests;

/// <summary>Bug sweep 3, batch J, worker side.</summary>
public class Sweep3JWorkerTests
{
    private static readonly (string Entity, string Base, bool Begin)?[] Map = [null, ("person", "GIVEN_NAME", true), ("address", "CITY", true)];

    /// <summary>C6: a NaN anywhere in a token's logits fails the device (NerGate then reports entitiesMissing) instead
    /// of silently tagging nothing; only the row maximum was checked.</summary>
    [Theory]
    [InlineData(0f, float.NaN, -5f)]       // NaN below the max: the max stayed finite, every p became NaN
    [InlineData(float.NaN, 2f, -5f)]
    [InlineData(1f, 2f, float.PositiveInfinity)]
    public void C6_non_finite_logit_anywhere_fails_the_device(float a, float b, float c)
    {
        Assert.Throws<DeviceFailedException>(() => PiiNer.Pick([a, b, c], Map));
    }

    [Fact]
    public void C6_finite_logits_still_tag()
    {
        Assert.Equal(1, PiiNer.Pick([-30f, 5f, -30f], Map).Tag);
    }
}
