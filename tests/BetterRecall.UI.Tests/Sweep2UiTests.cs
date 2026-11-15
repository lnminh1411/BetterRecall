// SPDX-License-Identifier: MIT
// Copyright (c) BetterRecall contributors
using System.Diagnostics;
using BetterRecall.UI.Logic;
using Xunit;

namespace BetterRecall.UI.Tests;

/// <summary>Bug sweep 2, batch E: "Store is not open." is not a lock (D1, S3), store banner strings (D3), agent exit (D39).</summary>
public sealed class Sweep2UiTests
{
    private static Dictionary<string, string> Strings()
    {
        var dir = new DirectoryInfo(AppContext.BaseDirectory);
        while (dir is not null && !File.Exists(Path.Combine(dir.FullName, "BetterRecall.slnx"))) dir = dir.Parent;
        Assert.NotNull(dir);
        var resw = System.Xml.Linq.XDocument.Load(Path.Combine(dir!.FullName, "src", "BetterRecall.UI", "Strings", "en-US", "Resources.resw"));
        return resw.Root!.Elements("data").ToDictionary(e => (string)e.Attribute("name")!, e => (string)e.Element("value")!);
    }

    [Fact]
    public void Store_not_open_is_its_own_kind_not_locked()
    {
        var r = AgentResult.FromEnvelope(false, default, "Store is not open.");
        Assert.Equal(AgentErrorKind.NotOpen, r.Kind);
        Assert.False(r.IsLocked);
        Assert.Equal(AgentErrorKind.Locked, AgentResult.FromEnvelope(false, default, "Store is locked.").Kind);
        Assert.Equal("Err_NotOpen", SearchViewModel.ErrorText(AgentErrorKind.NotOpen, r.Error).Key);
    }

    [Fact]
    public void Store_banner_strings_exist()
    {
        var s = Strings();
        foreach (var k in new[] { "Err_NotOpen", "StoreBar_KeyLostTitle", "StoreBar_KeyLostMessage", "StoreBar_OpenDanger", "StoreBar_ErrorTitle", "StoreBar_ErrorMessage" })
            Assert.False(string.IsNullOrWhiteSpace(s[k]), k);
        Assert.Contains("Danger zone", s["StoreBar_KeyLostMessage"]);
    }

    [Fact]
    public void Exit_watch_fires_for_a_gone_process_and_not_for_a_live_one()
    {
        using var w = new AgentExitWatch();
        int fired = 0;
        w.Exited += () => Interlocked.Increment(ref fired);
        w.Watch(Environment.ProcessId);
        Thread.Sleep(100);
        Assert.Equal(0, fired);

        using var p = Process.Start(new ProcessStartInfo("cmd.exe", "/c exit 0") { UseShellExecute = false, CreateNoWindow = true })!;
        w.Watch(p.Id);
        p.WaitForExit();
        var until = DateTime.UtcNow.AddSeconds(5);
        while (Volatile.Read(ref fired) == 0 && DateTime.UtcNow < until) Thread.Sleep(20);
        Assert.Equal(1, fired);
        w.Watch(p.Id);   // same pid again: no second event
        Thread.Sleep(50);
        Assert.Equal(1, fired);
    }
}
