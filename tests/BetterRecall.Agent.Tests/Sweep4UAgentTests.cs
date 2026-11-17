// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using BetterRecall.Agent.Host;
using BetterRecall.Agent.Replay;
using BetterRecall.Contracts;

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
}
