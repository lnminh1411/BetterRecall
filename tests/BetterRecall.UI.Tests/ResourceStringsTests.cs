// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Text.RegularExpressions;
using BetterRecall.UI.Logic;

namespace BetterRecall.UI.Tests;

public sealed class ResourceStringsTests
{
    [Fact]
    public void Every_schema_group_key_choice_and_warning_exists_in_resources()
    {
        var s = TestResources.Strings();

        foreach (var group in SettingsSchema.Groups)
        {
            Assert.True(s.ContainsKey("G_" + group), "G_" + group);
            Assert.False(string.IsNullOrWhiteSpace(s["G_" + group]), "G_" + group);
            Assert.True(s.ContainsKey("G_" + group + "_D"), "G_" + group + "_D");
        }

        foreach (var d in SettingsSchema.All)
        {
            Assert.True(s.ContainsKey(d.LabelKey), d.LabelKey);
            Assert.False(string.IsNullOrWhiteSpace(s[d.LabelKey]), d.LabelKey);
            if (d.WarningKey is not null)
                Assert.True(s.ContainsKey(d.WarningKey), d.WarningKey);
            if (d.Type == SettingType.Choice && d.Choices is not null)
            {
                foreach (var c in d.Choices)
                    Assert.True(s.ContainsKey(d.ChoiceKey(c)), d.ChoiceKey(c));
            }
        }

        // Settings groups whose descriptions are required on every entry.
        foreach (var key in new[]
        {
            "storage.minFreeGb", "storage.maxGb", "storage.overwrite",
            "retention.progressiveAfterDays", "retention.progressiveOverDays",
            "ai.share", "ai.chatMaxHits", "ai.chatMaxImages", "ai.dailyRequests", "ai.dailyTokens", "ai.chatHistory",
        })
        {
            var d = SettingsSchema.Find(key)!;
            Assert.True(s.ContainsKey(d.DescriptionKey), d.DescriptionKey);
        }
    }

    [Fact]
    public void Every_literal_LocalizedText_key_in_logic_exists_in_resources()
    {
        var s = TestResources.Strings();
        var logicDir = Path.Combine(TestResources.UiDir(), "Logic");
        var missing = new List<string>();
        int checkedCount = 0;

        foreach (var file in Directory.GetFiles(logicDir, "*.cs"))
        {
            var text = File.ReadAllText(file);
            // Every "Prefix_Key" literal: LocalizedText("..."), new("..."), ternaries and helpers that
            // return key strings. Prefixes ending in '_' (built keys) are skipped.
            foreach (Match m in Regex.Matches(text, @"""([A-Z][A-Za-z0-9]*_[A-Za-z0-9_]*[A-Za-z0-9])"""))
            {
                checkedCount++;
                var key = m.Groups[1].Value;
                if (!s.ContainsKey(key))
                    missing.Add($"{Path.GetFileName(file)}:{key}");
            }
        }

        Assert.True(checkedCount > 20, "Expected to scan literal LocalizedText keys in Logic/*.cs");
        Assert.Empty(missing);
    }

    [Fact]
    public void Key_copy_pins_placeholders_and_dead_keys_stay_gone()
    {
        var s = TestResources.Strings();

        // XAML / UI keys not referenced via LocalizedText in Logic/*.cs.
        foreach (var k in new[]
        {
            "Nav_Chat.Content", "Chat_Usage", "Chat_NoLimit", "Chat_Searching", "Chat_ApprovalTitle",
            "Chat_AllowChat", "Chat_DontShare", "Chat_ErrGeneric",
            "Chat_ErrLocked", "Chat_ErrStopped", "Chat_ErrLockedBody", "Chat_ErrStoppedBody",
            "Hw_KeyframeEncoder", "Reason_storage", "StorageBar_Title", "StorageBar_Message", "StorageBar_Open",
            "Danger_ExportTitle.Text", "Danger_ExportWarnBody", "Danger_Exported", "Danger_ExportNone",
            "Home_Connecting", "Home_Day", "Timeline_EmptyPending", "Chat_PageTitle.Text",
            "Chat_ApprovalInclude", "Audit_Empty.Text", "Key_Delete", "Nav_Settings.Content",
            "Err_NotOpen", "StoreBar_KeyLostTitle", "StoreBar_KeyLostMessage", "StoreBar_OpenDanger",
            "StoreBar_ErrorTitle", "StoreBar_ErrorMessage",
        })
        {
            Assert.True(s.ContainsKey(k), k);
            Assert.False(string.IsNullOrWhiteSpace(s[k]), k);
        }

        // Dead keys must stay removed.
        Assert.False(s.ContainsKey("Chat_Sources"));
        Assert.False(s.ContainsKey("Launcher_App.PlaceholderText"));

        // Required copy pins and placeholders.
        Assert.Contains("on-screen text reading", s["S_ai_share_D"]);
        Assert.Contains("Danger zone", s["StoreBar_KeyLostMessage"]);
        Assert.Contains("letters, digits, dots, dashes or underscores", s["Settings_PipeNameError"]);
        Assert.Contains("{0}", s["R_replay_unsupported"]);
        Assert.Contains("{1}", s["R_replay_unsupported"]);
        Assert.Contains("{0}", s["Chat_ApprovalInclude"]);
        Assert.Contains("{1}", s["Chat_ApprovalInclude"]);
    }

    /// <summary>T1: consent and security copy matches the current defaults.</summary>
    [Fact]
    public void Consent_copy_matches_the_defaults()
    {
        var s = TestResources.Strings();
        Assert.DoesNotContain("No text recognition", s["Consent_What4.Text"]);
        Assert.Contains("OCR", s["Consent_What4.Text"]);
        Assert.NotEqual("• BetterRecall makes no network connections.", s["Consent_Where3.Text"]);
        Assert.Contains("Cloud describe is off", s["Consent_Where3.Text"]);
        Assert.DoesNotContain("Windows Hello", s["S_security_requireUnlock"]);   // also true with the Password method
        Assert.DoesNotContain("Windows Hello", s["W_security_requireUnlock"]);
    }
}
