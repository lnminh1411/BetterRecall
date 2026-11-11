// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using BetterRecall.Agent.Capture;
using BetterRecall.Agent.Guardrails;
using BetterRecall.Agent.Host;
using BetterRecall.Contracts;

namespace BetterRecall.Agent.Tests;

public class DHashTests
{
    private static byte[] Gradient(int w, int h, bool leftToRight, int noiseSeed = 0)
    {
        var rnd = new Random(noiseSeed);
        var px = new byte[w * h * 4];
        for (int y = 0; y < h; y++)
            for (int x = 0; x < w; x++)
            {
                int v = leftToRight ? x * 255 / (w - 1) : 255 - x * 255 / (w - 1);
                if (noiseSeed != 0) v = Math.Clamp(v + rnd.Next(-3, 4), 0, 255);
                int i = (y * w + x) * 4;
                px[i] = px[i + 1] = px[i + 2] = (byte)v;
                px[i + 3] = 255;
            }
        return px;
    }

    [Fact]
    public void Identical_images_hash_equal()
    {
        var a = Gradient(320, 200, true);
        Assert.Equal(DHash.Compute(a, 320, 200, 320 * 4), DHash.Compute(a, 320, 200, 320 * 4));
    }

    [Fact]
    public void Small_noise_stays_within_threshold_and_inverse_is_far()
    {
        var a = Gradient(320, 200, true);
        var noisy = Gradient(320, 200, true, noiseSeed: 7);
        var inverse = Gradient(320, 200, false);
        ulong ha = DHash.Compute(a, 320, 200, 1280), hn = DHash.Compute(noisy, 320, 200, 1280), hi = DHash.Compute(inverse, 320, 200, 1280);
        Assert.True(DHash.Distance(ha, hn) < 6);
        Assert.Equal(64, DHash.Distance(ha, hi));
    }

    [Fact]
    public void Respects_stride_padding()
    {
        var a = Gradient(100, 80, true);
        int stride = 100 * 4 + 64;
        var padded = new byte[stride * 80];
        for (int y = 0; y < 80; y++) Array.Copy(a, y * 400, padded, y * stride, 400);
        Assert.Equal(DHash.Compute(a, 100, 80, 400), DHash.Compute(padded, 100, 80, stride));
    }

    [Fact]
    public void Dedup_stages_run_cheapest_first()
    {
        var d = new DedupFilter();
        Assert.Equal(DedupVerdict.Keep, d.PreCheck(false, 0, 0.5, false)); // nothing kept yet
        d.Accept(0xFFUL, "app\u0001title");
        Assert.Equal(DedupVerdict.SkipNoFrame, d.PreCheck(false, 100, 0.5, false));
        Assert.Equal(DedupVerdict.SkipDirtyArea, d.PreCheck(true, 0.1, 0.5, false));
        Assert.Equal(DedupVerdict.Keep, d.PreCheck(true, 3, 0.5, false));
        Assert.Equal(DedupVerdict.Keep, d.PreCheck(false, 0, 0.5, contextChanged: true));
        Assert.Equal(DedupVerdict.SkipSimilar, d.HashCheck(0xFFUL | 0x100UL, 6, "app\u0001title"));
        Assert.Equal(DedupVerdict.Keep, d.HashCheck(0xFFFF_FF00UL, 6, "app\u0001title"));
        Assert.Equal(DedupVerdict.Keep, d.HashCheck(0xFFUL, 6, "app\u0001other title"));
        d.Reset();
        Assert.Null(d.LastHash);
    }
}

public class GuardrailTests
{
    private static WindowInfo W(string exe, string title = "Doc") => new(1, 42, exe, title, "Chrome_WidgetWin_1");

    [Theory]
    [InlineData(@"C:\Program Files\KeePassXC\KeePassXC.exe", (int)GuardReason.DenyProcess)]
    [InlineData(@"C:\Windows\System32\mstsc.exe", (int)GuardReason.DenyProcess)]
    [InlineData(@"C:\Tools\notepad.exe", (int)GuardReason.None)]
    public void Process_deny_list(string exe, int expectedReason)
    {
        var expected = (GuardReason)expectedReason;
        var p = new PrivacySettings { BrowserBadgeDetection = false };
        var d = GuardrailPolicy.Evaluate(W(exe), p, false, false, BadgeResult.NotChecked);
        Assert.Equal(expected == GuardReason.None, d.Allow);
        Assert.Equal(expected, d.Reason);
    }

    [Fact]
    public void Path_prefix_matches_on_boundary_only()
    {
        string[] prefixes = [@"C:\Secret"];
        Assert.True(GuardrailPolicy.IsDeniedPath(@"C:\Secret\app.exe", prefixes));
        Assert.True(GuardrailPolicy.IsDeniedPath(@"c:\secret\sub\APP.exe", prefixes));
        Assert.False(GuardrailPolicy.IsDeniedPath(@"C:\SecretStuff\app.exe", prefixes));
        Assert.False(GuardrailPolicy.IsDeniedPath("", prefixes));
    }

    [Fact]
    public void Title_patterns_are_case_insensitive_with_globs()
    {
        string[] pats = ["InPrivate", "bank*statement"];
        Assert.True(GuardrailPolicy.TitleMatches("New tab - [InPrivate] - Microsoft Edge", pats));
        Assert.True(GuardrailPolicy.TitleMatches("My BANK - March Statement.pdf", pats));
        Assert.False(GuardrailPolicy.TitleMatches("Statement of bank", pats));
        Assert.Equal("", GuardrailPolicy.FilterTitle("x - inprivate", new PrivacySettings { DenyTitlePatterns = ["InPrivate"] }));
        Assert.Equal("Inbox", GuardrailPolicy.FilterTitle("Inbox", new PrivacySettings()));
    }

    [Fact]
    public void Session_lock_affinity_and_badge()
    {
        var p = new PrivacySettings();
        var chrome = W(@"C:\Program Files\Google\Chrome\Application\chrome.exe", "Google");
        Assert.Equal(GuardReason.SessionLocked, GuardrailPolicy.Evaluate(chrome, p, true, false, BadgeResult.NoBadge).Reason);
        Assert.Equal(GuardReason.NoForeground, GuardrailPolicy.Evaluate(null, p, false, false, BadgeResult.NoBadge).Reason);
        Assert.Equal(GuardReason.DisplayAffinity, GuardrailPolicy.Evaluate(chrome, p, false, true, BadgeResult.NoBadge).Reason);
        Assert.Equal(GuardReason.PrivateBadge, GuardrailPolicy.Evaluate(chrome, p, false, false, BadgeResult.Private).Reason);
        // A failed or missing check drops the frame.
        Assert.Equal(GuardReason.BadgeCheckFailed, GuardrailPolicy.Evaluate(chrome, p, false, false, BadgeResult.CheckFailed).Reason);
        Assert.Equal(GuardReason.BadgeCheckFailed, GuardrailPolicy.Evaluate(chrome, p, false, false, BadgeResult.NotChecked).Reason);
        Assert.True(GuardrailPolicy.Evaluate(chrome, p, false, false, BadgeResult.NoBadge).Allow);
        // Settings switch the checks off.
        var off = new PrivacySettings { PauseOnLock = false, HonorDisplayAffinity = false, BrowserBadgeDetection = false };
        Assert.True(GuardrailPolicy.Evaluate(chrome, off, true, true, BadgeResult.CheckFailed).Allow);
    }

    [Fact]
    public void Deny_list_wins_over_later_checks()
    {
        var p = new PrivacySettings();
        var d = GuardrailPolicy.Evaluate(W(@"C:\x\1Password.exe"), p, false, true, BadgeResult.Private);
        Assert.Equal(GuardReason.DenyProcess, d.Reason);
    }
}

public class HotkeyParserTests
{
    [Theory]
    [InlineData("Ctrl+Alt+Space", Hotkey.ModControl | Hotkey.ModAlt, 0x20u)]
    [InlineData("Ctrl+Alt+P", Hotkey.ModControl | Hotkey.ModAlt, (uint)'P')]
    [InlineData("ctrl + alt + shift + x", Hotkey.ModControl | Hotkey.ModAlt | Hotkey.ModShift, (uint)'X')]
    [InlineData("Win+F12", Hotkey.ModWin, 0x7Bu)]
    [InlineData("Alt+1", Hotkey.ModAlt, (uint)'1')]
    [InlineData("Ctrl+Plus", Hotkey.ModControl, 0xBBu)]
    [InlineData("Ctrl++", Hotkey.ModControl, 0xBBu)]
    [InlineData("Shift+PageDown", Hotkey.ModShift, 0x22u)]
    [InlineData("F9", 0u, 0x78u)]
    [InlineData("Ctrl+NumPad7", Hotkey.ModControl, 0x67u)]
    [InlineData("Win+Shift+PrintScreen", Hotkey.ModWin | Hotkey.ModShift, 0x2Cu)]
    [InlineData("Ctrl+Alt+;", Hotkey.ModControl | Hotkey.ModAlt, 0xBAu)]
    [InlineData("Ctrl+Alt+=", Hotkey.ModControl | Hotkey.ModAlt, 0xBBu)]
    [InlineData("Ctrl+Alt+-", Hotkey.ModControl | Hotkey.ModAlt, 0xBDu)]
    [InlineData("Ctrl+Alt+\\", Hotkey.ModControl | Hotkey.ModAlt, 0xDCu)]
    [InlineData("Ctrl+Alt+'", Hotkey.ModControl | Hotkey.ModAlt, 0xDEu)]
    [InlineData("Ctrl+Alt+`", Hotkey.ModControl | Hotkey.ModAlt, 0xC0u)]
    [InlineData("Alt+Pause", Hotkey.ModAlt, 0x13u)]
    public void Parses(string text, uint mods, uint vk)
    {
        var hk = HotkeyParser.Parse(text);
        Assert.NotNull(hk);
        Assert.Equal(mods, hk!.Value.Modifiers);
        Assert.Equal(vk, hk.Value.VirtualKey);
    }

    [Theory]
    [InlineData("")]
    [InlineData("Ctrl+Alt")]
    [InlineData("Ctrl+Ctrl+A")]
    [InlineData("Ctrl+A+B")]
    [InlineData("Ctrl+Foo")]
    [InlineData("F25")]
    [InlineData(null)]
    public void Rejects(string? text) => Assert.Null(HotkeyParser.Parse(text));

    [Fact]
    public void Round_trips_format()
    {
        foreach (var s in new[] { "Ctrl+Alt+Space", "Ctrl+Alt+Shift+X", "Win+F12", "Ctrl+NumPad3", "Alt+PageDown", "Ctrl+Alt+;", "Ctrl+Alt+=", "Ctrl+Alt+-", "Shift+/", "Ctrl+Alt+[", "Ctrl+Alt+\\", "Ctrl+'", "Win+PrintScreen", "Ctrl+Insert" })
            Assert.Equal(s, HotkeyParser.Format(HotkeyParser.Parse(s)!.Value));
    }
}

public class AgentStateTests
{
    [Fact]
    public void Read_gate_opens_for_timeout_and_closes()
    {
        var now = new DateTimeOffset(2026, 11, 10, 12, 0, 0, TimeSpan.Zero);
        var gate = new ReadGate(() => now);
        Assert.False(gate.IsOpen(requireUnlock: true));
        Assert.True(gate.IsOpen(requireUnlock: false));
        gate.Open(15);
        Assert.True(gate.IsOpen(true));
        now = now.AddMinutes(14.9);
        Assert.True(gate.IsOpen(true));
        now = now.AddMinutes(0.2);
        Assert.False(gate.IsOpen(true));
        gate.Open(15);
        gate.Close();
        Assert.False(gate.IsOpen(true));
    }

    [Fact]
    public void Pause_reasons_priority_and_expiry()
    {
        var now = DateTimeOffset.UnixEpoch;
        var p = new PauseController(() => now);
        Assert.Null(p.Current().Reason);
        p.Set("fullscreen");
        p.Set("user", now.AddMinutes(15));
        Assert.Equal("user", p.Current().Reason);
        Assert.False(p.ContinuousPaused(duringFullscreen: true) == false); // user pause stops the replay too
        p.Set("consent");
        Assert.Equal("consent", p.Current().Reason);
        p.Clear("consent");
        now = now.AddMinutes(16);
        Assert.Equal("fullscreen", p.Current().Reason);
        Assert.True(p.KeyframesPaused);
        Assert.False(p.ContinuousPaused(duringFullscreen: true));
        Assert.True(p.ContinuousPaused(duringFullscreen: false));
    }
}
