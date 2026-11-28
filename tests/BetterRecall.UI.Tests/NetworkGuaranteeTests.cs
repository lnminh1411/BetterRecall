// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Text.Json.Nodes;
using BetterRecall.Contracts;
using BetterRecall.UI.Logic;

namespace BetterRecall.UI.Tests;

public sealed class NetworkGuaranteeTests
{
    [Fact]
    public void Turning_cloud_on_asks_first_turning_off_does_not()
    {
        Assert.True(NetworkGuarantee.NeedsConfirm("ai.enabled", on: true, savedOn: false));
        Assert.False(NetworkGuarantee.NeedsConfirm("ai.enabled", on: false, savedOn: false));
        Assert.False(NetworkGuarantee.NeedsConfirm("ai.enabled", on: false, savedOn: true));
        Assert.False(NetworkGuarantee.NeedsConfirm("ai.enabled", on: true, savedOn: true));   // already on: nothing new ends
        Assert.False(NetworkGuarantee.NeedsConfirm("replay.enabled", on: true, savedOn: false));
    }

    [Fact]
    public void Guarantee_state_follows_saved_ai_enabled()
    {
        Assert.False(NetworkGuarantee.IsOff(null));
        Assert.False(NetworkGuarantee.IsOff(JsonNode.Parse("{}")!.AsObject()));
        Assert.False(NetworkGuarantee.IsOff(JsonNode.Parse("{\"ai\":{\"enabled\":false}}")!.AsObject()));
        Assert.True(NetworkGuarantee.IsOff(JsonNode.Parse("{\"ai\":{\"enabled\":true}}")!.AsObject()));
        Assert.Equal("S_ai_provider_claude", NetworkGuarantee.ProviderLabelKey(JsonNode.Parse("{\"ai\":{\"provider\":\"claude\"}}")!.AsObject()));
        Assert.Equal("S_ai_provider_gemini", NetworkGuarantee.ProviderLabelKey(JsonNode.Parse("{\"ai\":{\"provider\":\"evil\"}}")!.AsObject()));
    }

    [Fact]
    public void Warning_texts_name_the_guarantee()
    {
        var s = TestResources.Strings();
        var d = SettingsSchema.Find(NetworkGuarantee.SettingKey)!;
        Assert.Equal("W_ai_enabled", d.WarningKey);
        Assert.Contains("no-network guarantee", s[d.WarningKey!], StringComparison.OrdinalIgnoreCase);
        Assert.Contains("guarantee", s[NetworkGuarantee.ConfirmTitleKey], StringComparison.OrdinalIgnoreCase);
        var body = s[NetworkGuarantee.ConfirmBodyKey];
        Assert.StartsWith("Turning this on ends the no-network guarantee", body);
        Assert.Contains("{0}", body);   // the provider is named
        Assert.Contains("restore the guarantee", body);
        Assert.Contains("No-network guarantee off", s[NetworkGuarantee.OffLineKey]);
        Assert.True(s.ContainsKey(NetworkGuarantee.ConfirmButtonKey));
        foreach (var p in AiDefaults.Providers)
            Assert.True(s.ContainsKey(NetworkGuarantee.ProviderLabelKey(new JsonObject { ["ai"] = new JsonObject { ["provider"] = p } })), p);
    }
}
