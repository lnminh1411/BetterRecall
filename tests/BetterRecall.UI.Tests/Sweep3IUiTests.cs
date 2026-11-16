// SPDX-License-Identifier: MIT
// Copyright (c) BetterRecall contributors
using System.Text.RegularExpressions;
using BetterRecall.UI.Logic;

namespace BetterRecall.UI.Tests;

/// <summary>Bug sweep 3, batch I: D1 (a style based on a key WinUI does not have broke every list editor).</summary>
public class Sweep3IUiTests
{
    /// <summary>WinUI 3 style keys a BasedOn may name: found in Microsoft.UI.Xaml.Controls.pri and the
    /// Microsoft.ui.xaml.resources dlls (D1 byte search), or already used by pages that render. "DefaultListViewStyle"
    /// is not one of them. Add a key here only after checking it exists.</summary>
    private static readonly HashSet<string> WinUiStyleKeys =
    [
        "DefaultButtonStyle", "AccentButtonStyle", "DefaultListViewItemStyle", "DefaultComboBoxStyle",
        "TitleTextBlockStyle", "BodyTextBlockStyle", "CaptionTextBlockStyle", "BodyStrongTextBlockStyle",
    ];

    private static string UiDir()
    {
        for (var d = new DirectoryInfo(AppContext.BaseDirectory); d is not null; d = d.Parent)
            if (File.Exists(Path.Combine(d.FullName, "BetterRecall.slnx"))) return Path.Combine(d.FullName, "src", "BetterRecall.UI");
        throw new DirectoryNotFoundException("repo root not found");
    }

    [Fact]
    public void D1_every_BasedOn_names_a_key_that_exists()
    {
        var files = Directory.GetFiles(UiDir(), "*.xaml", SearchOption.AllDirectories);
        var defined = new HashSet<string>(files.SelectMany(f => Regex.Matches(File.ReadAllText(f), "x:Key=\"([^\"]+)\"").Select(m => m.Groups[1].Value)));
        var bad = files
            .SelectMany(f => Regex.Matches(File.ReadAllText(f), "BasedOn=\"\\{StaticResource ([^}\\s]+)\\}\"").Select(m => (File: Path.GetFileName(f), Key: m.Groups[1].Value)))
            .Where(b => !defined.Contains(b.Key) && !WinUiStyleKeys.Contains(b.Key))
            .ToList();
        Assert.Empty(bad);
    }

    [Fact]
    public void D1_styles_the_editors_look_up_in_code_are_defined()
    {
        var app = File.ReadAllText(Path.Combine(UiDir(), "App.xaml"));
        var code = string.Concat(Directory.GetFiles(Path.Combine(UiDir(), "Views"), "*.cs").Select(File.ReadAllText));
        var used = Regex.Matches(code, "Application\\.Current\\.Resources\\[\"([^\"]+)\"\\]").Select(m => m.Groups[1].Value).Distinct().ToList();
        Assert.Contains("OutlinedListViewStyle", used);
        Assert.All(used, k => Assert.True(app.Contains("x:Key=\"" + k + "\"") || WinUiStyleKeys.Contains(k), k));
    }

    [Fact]
    public void D1_swallowed_exceptions_log_type_and_stack_but_never_the_message()
    {
        Exception ex;
        try { throw new InvalidOperationException("secret OCR text 4111 1111", new FormatException("inner secret")); }
        catch (Exception e) { ex = e; }
        var entry = UiErrorLog.Format(ex, new DateTimeOffset(2026, 11, 16, 10, 0, 0, TimeSpan.Zero));
        Assert.Contains("System.InvalidOperationException", entry);
        Assert.Contains("inner System.FormatException", entry);
        Assert.Contains(nameof(D1_swallowed_exceptions_log_type_and_stack_but_never_the_message), entry);   // stack frame
        Assert.DoesNotContain("secret", entry);
        Assert.DoesNotContain("4111", entry);

        var dir = Path.Combine(Path.GetTempPath(), "br-ui-errlog-" + Guid.NewGuid().ToString("N"));
        try
        {
            UiErrorLog.Append(dir, ex);
            UiErrorLog.Append(dir, null);
            var text = File.ReadAllText(Path.Combine(dir, UiErrorLog.FileName));
            Assert.Equal(2, Regex.Matches(text, " unhandled\n").Count);
        }
        finally { try { Directory.Delete(dir, true); } catch { } }
    }
}
