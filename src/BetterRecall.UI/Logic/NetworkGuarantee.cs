// SPDX-License-Identifier: MIT
// Copyright (c) BetterRecall contributors
using System.Text.Json.Nodes;
using BetterRecall.Contracts;

namespace BetterRecall.UI.Logic;

/// <summary>
/// BetterRecall's no-network guarantee holds only while cloud describe (ai.enabled) is off. Turning it on asks first
/// (danger dialog, Cancel default); while it is on the UI says the guarantee is off. Turning it off needs no dialog.
/// </summary>
public static class NetworkGuarantee
{
    public const string SettingKey = "ai.enabled";
    /// <summary>Dialog title; the body's first line says the guarantee ends.</summary>
    public const string ConfirmTitleKey = "Ai_ConfirmTitle";
    /// <summary>Body, one format argument: the provider's display name.</summary>
    public const string ConfirmBodyKey = "Ai_ConfirmBody";
    public const string ConfirmButtonKey = "Ai_ConfirmTurnOn";
    /// <summary>Persistent line while ai.enabled is saved on (settings AI group, Home status).</summary>
    public const string OffLineKey = "Ai_GuaranteeOff";

    /// <summary>True when switching key on must show the confirmation first. Off, or already saved on: no dialog.</summary>
    public static bool NeedsConfirm(string key, bool on, bool savedOn) =>
        on && !savedOn && string.Equals(key, SettingKey, StringComparison.OrdinalIgnoreCase);

    /// <summary>True while the saved settings have cloud describe on, so the guarantee is off.</summary>
    public static bool IsOff(JsonObject? settings) =>
        settings is not null && SettingsSchema.Get(settings, SettingKey)?.GetValue<bool>() == true;

    /// <summary>Resource key of the provider's display name ("S_ai_provider_gemini").</summary>
    public static string ProviderLabelKey(JsonObject? settings)
    {
        var p = settings is null ? null : SettingsSchema.Get(settings, "ai.provider")?.GetValue<string>();
        return SettingsSchema.Find("ai.provider")!.ChoiceKey(p is not null && AiDefaults.Providers.Contains(p) ? p : AiDefaults.Gemini);
    }
}
