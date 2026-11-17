// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using BetterRecall.Agent.Host;

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
}
