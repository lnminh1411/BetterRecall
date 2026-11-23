// SPDX-License-Identifier: MIT
// Copyright (c) BetterRecall contributors
using System.Diagnostics;
using BetterRecall.UI.Logic;

namespace BetterRecall.UI.Tests;

/// <summary>Agent process exit detection and UI exit policy (D39, C6).</summary>
public sealed class AgentExitWatchTests
{
    [Fact]
    public void Exit_policy_requires_dead_agent_and_no_visible_windows()
    {
        Assert.False(UiExitPolicy.ShouldExit(agentExited: false, launcherVisible: false, mainVisible: false));
        Assert.False(UiExitPolicy.ShouldExit(agentExited: true, launcherVisible: true, mainVisible: false));
        Assert.False(UiExitPolicy.ShouldExit(agentExited: true, launcherVisible: false, mainVisible: true));
        Assert.True(UiExitPolicy.ShouldExit(agentExited: true, launcherVisible: false, mainVisible: false));
    }

    [Fact]
    public void Exit_watch_tracks_live_gone_and_reattached_processes()
    {
        using var w = new AgentExitWatch();
        int fired = 0;
        w.Exited += () => Interlocked.Increment(ref fired);

        // Live process does not fire.
        w.Watch(Environment.ProcessId);
        Thread.Sleep(100);
        Assert.Equal(0, fired);
        Assert.False(w.HasExited);
        Assert.True(w.IsAlive());

        // Real exiting process fires once; watching the same dead PID again does not fire a second time.
        using var p = Process.Start(new ProcessStartInfo("cmd.exe", "/c exit 0") { UseShellExecute = false, CreateNoWindow = true })!;
        w.Watch(p.Id);
        Assert.True(p.WaitForExit(5000), "cmd.exe did not exit within 5 s");
        var until = DateTime.UtcNow.AddSeconds(5);
        while (Volatile.Read(ref fired) == 0 && DateTime.UtcNow < until) Thread.Sleep(20);
        Assert.Equal(1, fired);
        w.Watch(p.Id);
        Thread.Sleep(50);
        Assert.Equal(1, fired);

        // Watching a dead/non-existent PID marks HasExited and !IsAlive(), then re-attaching to a live PID resets it.
        w.Watch(int.MaxValue);
        Assert.True(w.HasExited);
        Assert.False(w.IsAlive());
        Assert.Equal(2, fired);

        w.Watch(Environment.ProcessId);
        Assert.False(w.HasExited);
        Assert.True(w.IsAlive());
    }
}
