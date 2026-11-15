// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Text.Json;
using BetterRecall.Contracts;
using BetterRecall.Core.Crypto;

namespace BetterRecall.Core.Tests;

/// <summary>Bug sweep 2, batch G: Core fixes (B13, B17, B22, C19, N3).</summary>
public class Sweep2GTests
{
    // ---------------- C19 ----------------

    [Theory]
    [InlineData("auto", true)]
    [InlineData("vi", true)]
    [InlineData("haw", true)]
    [InlineData("xx", false)]
    [InlineData("eng", false)]
    [InlineData("zz", false)]
    public void Audio_language_is_checked_against_whisper_codes(string lang, bool ok)
    {
        var s = new Settings();
        s.Audio.Language = lang;
        Assert.Equal(ok ? null : "audio.language", SettingsStore.Validate(s));
    }

    [Fact]
    public void Settings_file_with_an_unknown_language_keeps_auto()
    {
        using var h = new TempHome(null);
        File.WriteAllText(DataPaths.SettingsFile(h.Path), """{ "audio": { "language": "xx" }, "index": { "dim": 512 } }""");
        var store = SettingsStore.Load(h.Path);
        Assert.Equal("auto", store.Current.Audio.Language);
        Assert.Equal(512, store.Current.Index.Dim);
    }

    // ---------------- N3 ----------------

    [Fact]
    public void Passphrase_tries_are_throttled_like_the_unlock_password()
    {
        using var h = new TempHome(null);
        var s = new Settings();
        s.Security.KeyProtection = "passphrase";
        using (var v = new KeyVault(h.Path, () => s, probeHello: false))
            Assert.True(v.UnlockWithPassphrase("correct horse"));
        using var vault = new KeyVault(h.Path, () => s, probeHello: false);
        var now = DateTimeOffset.UtcNow;
        vault.Now = () => now;
        for (int i = 0; i < UnlockPassword.FreeTries; i++) Assert.False(vault.UnlockWithPassphrase("wrong " + i));
        Assert.Equal(30, vault.PassphraseRetryAfterSeconds);
        // Throttled: even the right passphrase is refused without an Argon2id run.
        Assert.False(vault.UnlockWithPassphrase("correct horse"));
        Assert.False(vault.KeyLoaded);
        now = now.AddSeconds(31);
        Assert.Equal(0, vault.PassphraseRetryAfterSeconds);
        Assert.True(vault.UnlockWithPassphrase("correct horse"));
        Assert.Equal(0, vault.PassphraseRetryAfterSeconds);
    }

    // ---------------- B17 ----------------

    [Fact]
    public void Durable_move_replaces_the_target()
    {
        using var h = new TempHome(null);
        var a = Path.Combine(h.Path, "a.tmp");
        var b = Path.Combine(h.Path, "b.bin");
        File.WriteAllText(b, "old");
        File.WriteAllText(a, "new");
        Native.Win32.MoveDurable(a, b);
        Assert.False(File.Exists(a));
        Assert.Equal("new", File.ReadAllText(b));
        Assert.Throws<IOException>(() => Native.Win32.MoveDurable(a, b));
    }

    // ---------------- B13 / B22 ----------------

    [Fact]
    public void Maintenance_sweeps_keyframes_whose_row_is_gone()
    {
        using var t = new TempStore();
        var kept = t.Store.AddScreenCapture(Caps.Screen(DateTimeOffset.UtcNow.AddMinutes(-5), title: "kept"));
        Assert.Contains(t.Store.TakePendingEmbeddings(10), j => j.Id == kept);
        t.Store.SetEmbedding(kept, Vec.Seeded(1), Vec.Info);   // ready, so GetKeyframe serves it
        var lost = t.Store.AddScreenCapture(Caps.Screen(DateTimeOffset.UtcNow.AddMinutes(-4), title: "crashed delete"));
        Assert.Equal(2, t.Store.KeyframeCountForTest());
        t.Store.DeleteRowKeepImageForTest(lost);
        Assert.Equal(2, t.Store.KeyframeCountForTest());
        t.Store.RunMaintenance(new Settings());
        Assert.Equal(1, t.Store.KeyframeCountForTest());
        Assert.NotNull(t.Store.GetKeyframe(kept));
    }

    [Fact]
    public async Task Purge_and_keyframe_reads_run_while_maintenance_runs()
    {
        using var t = new TempStore();
        var now = DateTimeOffset.UtcNow;
        var ids = new List<string>();
        for (int i = 0; i < 200; i++) ids.Add(t.Store.AddScreenCapture(Caps.Screen(now.AddDays(-40).AddSeconds(i), title: "old " + i)));
        var recent = t.Store.AddScreenCapture(Caps.Screen(now.AddMinutes(-1), title: "recent"));
        var s = new Settings();
        s.Retention.Days = 30;
        var maintenance = Task.Run(() => t.Store.RunMaintenance(s));
        var purge = Task.Run(() => t.Store.Purge(PurgeScope.Last(TimeSpan.FromMinutes(10))));
        var read = Task.Run(() => t.Store.GetKeyframe(recent));
        await Task.WhenAll(maintenance, purge, read).WaitAsync(TimeSpan.FromSeconds(60));
        Assert.Equal(1, await purge);
        Assert.Equal(0, t.Store.KeyframeCountForTest());
        Assert.All(ids, id => Assert.Null(t.Store.Get(id)));
    }
}
