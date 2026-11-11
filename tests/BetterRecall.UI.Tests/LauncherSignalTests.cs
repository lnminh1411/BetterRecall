// SPDX-License-Identifier: MIT
// Copyright (c) BetterRecall contributors
using BetterRecall.UI.Logic;

namespace BetterRecall.UI.Tests;

public class LauncherSignalTests
{
    [Fact]
    public async Task SetEvent_RunsCallback_EachTime()
    {
        var name = @"Local\BetterRecall.UI.ShowLauncher.test." + Guid.NewGuid().ToString("N");
        int count = 0;
        var fired = new SemaphoreSlim(0);
        using var signal = new LauncherSignal(name, () => { Interlocked.Increment(ref count); fired.Release(); });
        Assert.True(signal.CreatedNew);

        using var opener = EventWaitHandle.OpenExisting(name);   // as the agent would
        opener.Set();
        Assert.True(await fired.WaitAsync(TimeSpan.FromSeconds(2)));
        opener.Set();
        Assert.True(await fired.WaitAsync(TimeSpan.FromSeconds(2)));
        Assert.Equal(2, count);
    }

    [Fact]
    public void Dispose_StopsWaiting()
    {
        var name = @"Local\BetterRecall.UI.ShowLauncher.test." + Guid.NewGuid().ToString("N");
        int count = 0;
        var signal = new LauncherSignal(name, () => Interlocked.Increment(ref count));
        signal.Dispose();
        Assert.False(EventWaitHandle.TryOpenExisting(name, out _));
        Assert.Equal(0, count);
    }
}
