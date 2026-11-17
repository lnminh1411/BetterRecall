// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using BetterRecall.Agent.Host;
using BetterRecall.Agent.Replay;
using BetterRecall.Contracts;
using BetterRecall.Core;
using BetterRecall.Core.Store;

namespace BetterRecall.Agent.Tests;

/// <summary>Bug sweep 4, batch U (control plane): agent side.</summary>
public sealed class Sweep4UAgentTests
{
    // ---- B11 (D5): a script's resume clears only the IPC pause

    [Fact]
    public void B11_ipc_resume_keeps_the_users_pause()
    {
        var p = new PauseController();
        p.Set("user");
        p.Set("ipc", DateTimeOffset.UtcNow.AddMinutes(1));
        AgentHost.ClearForResume(p, "ipc");
        Assert.True(p.Has("user"));
        Assert.False(p.Has("ipc"));
        Assert.Equal("user", p.Current().Reason);
    }

    [Fact]
    public void B11_user_resume_clears_both_pauses()
    {
        var p = new PauseController();
        p.Set("user");
        p.Set("ipc");
        AgentHost.ClearForResume(p, "user");
        Assert.False(p.Has("user"));
        Assert.False(p.Has("ipc"));
        Assert.Null(p.Current().Reason);
    }

    // ---- review S5: Core maps these exact messages to its purge answer (CommandProcessor.ReplayPurgeRefusals)

    [Fact]
    public async Task S5_replay_purge_refusals_keep_the_messages_core_matches()
    {
        var home = Path.Combine(Environment.GetEnvironmentVariable("BR_TEST_TMP") is { Length: > 0 } t ? t : Path.GetTempPath(), "br-s5-" + Guid.NewGuid().ToString("N")[..8]);
        Directory.CreateDirectory(home);
        try
        {
            bool purging = true;
            var lib = new ReplayLibrary(home, () => new Settings(), null) { PurgingAll = () => purging };
            var path = Path.Combine(home, "clip.mp4");
            var e = await Assert.ThrowsAsync<InvalidOperationException>(() => lib.WriteClipAsync(path, false, _ => 1, CancellationToken.None));
            Assert.Equal("Purge all is running.", e.Message);

            purging = false;
            e = await Assert.ThrowsAsync<InvalidOperationException>(() => lib.WriteClipAsync(path, false, s => { lib.OnPurgeAll(); return 1; }, CancellationToken.None));
            Assert.Equal("A purge ran during the replay save; the clip was discarded.", e.Message);
            Assert.False(File.Exists(path));
        }
        finally { Directory.Delete(home, true); }
    }

    // ---- S11: no retention or thinning while settings.json is unreadable

    [Fact]
    public void S11_retention_waits_for_a_readable_settings_file_and_runs_after_reset()
    {
        var home = Path.Combine(Environment.GetEnvironmentVariable("BR_TEST_TMP") is { Length: > 0 } t ? t : Path.GetTempPath(), "br-s11-" + Guid.NewGuid().ToString("N")[..8]);
        Directory.CreateDirectory(home);
        try
        {
            File.WriteAllText(DataPaths.SettingsFile(home), "{\"retention\":{\"days\":3650}} x");
            var settings = SettingsStore.Load(home);
            Assert.NotNull(settings.LoadError);
            var key = System.Security.Cryptography.RandomNumberGenerator.GetBytes(32);
            using (var store = new EncryptedStore(home, () => (byte[])key.Clone()))
            {
                var id = store.AddScreenCapture(new NewScreenCapture(DateTimeOffset.UtcNow.AddDays(-400), "app", "", "old", 0, 10, 10, 0, [1, 2, 3], "image/webp", true));
                var v = new float[768];
                v[0] = 1;
                store.SetEmbedding(id, v, new EmbeddingInfo("fake-gemma", "r1", 768));   // indexed: counted by Stats
                bool logged = false;
                store.RunMaintenance(settings.Current, AgentHost.RetentionAllowed(settings, ref logged));
                Assert.True(logged);
                Assert.Equal(1, store.Stats().Rows);   // the defaults in use would have deleted it

                Assert.True(settings.ResetUnreadable());
                store.RunMaintenance(settings.Current, AgentHost.RetentionAllowed(settings, ref logged));
                Assert.False(logged);
                Assert.Equal(0, store.Stats().Rows);
            }
            Microsoft.Data.Sqlite.SqliteConnection.ClearAllPools();
        }
        finally { try { Directory.Delete(home, true); } catch (IOException) { } }
    }
}
