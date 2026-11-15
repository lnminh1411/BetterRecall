// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using BetterRecall.Contracts;
using BetterRecall.Core.Store;

namespace BetterRecall.Core.Tests;

/// <summary>Bug sweep 2, batch F: one purge signal from the store to its listeners (N5) and the purge fence (B12, N4).</summary>
public sealed class PurgeSignalTests : IDisposable
{
    private readonly TempStore _t = new();
    private readonly List<PurgeNotice> _seen = [];

    public PurgeSignalTests() => _t.Store.Purged += _seen.Add;

    public void Dispose() => _t.Dispose();

    private EncryptedStore S => _t.Store;

    [Fact]
    public void Purge_last_tells_the_ids_and_the_window()
    {
        var now = DateTimeOffset.UtcNow;
        var old = S.AddScreenCapture(Caps.Screen(now.AddHours(-2)));
        var recent = S.AddScreenCapture(Caps.Screen(now.AddSeconds(-30)));
        var scope = PurgeScope.Last(TimeSpan.FromMinutes(15));
        Assert.Equal(1, S.Purge(scope));
        var n = Assert.Single(_seen);
        Assert.Equal("purge", n.Reason);
        Assert.True(n.ByUser);
        Assert.Equal([recent], n.Ids);
        Assert.Equal(scope.From, n.From);
        Assert.Equal(scope.To, n.To);
        Assert.InRange(n.Newest!.Value, now.AddSeconds(-31), now.AddSeconds(-29));
        Assert.DoesNotContain(old, n.Ids);
    }

    [Fact]
    public void Purge_of_an_empty_window_still_tells_so_live_buffers_are_cleared()
    {
        // The replay ring and samples hold the window even when nothing was stored in it yet.
        S.Purge(PurgeScope.Last(TimeSpan.FromMinutes(5)));
        var n = Assert.Single(_seen);
        Assert.Empty(n.Ids);
        Assert.NotNull(n.From);
    }

    [Theory]
    [InlineData("app")]
    [InlineData("site")]
    [InlineData("id")]
    public void Scoped_purges_tell_the_ids_without_a_window(string kind)
    {
        var id = S.AddScreenCapture(Caps.Screen(DateTimeOffset.UtcNow.AddSeconds(-5), "signal.exe") with { Site = "chat.example.org" });
        S.AddScreenCapture(Caps.Screen(DateTimeOffset.UtcNow.AddSeconds(-4), "notepad.exe"));
        var scope = kind switch { "app" => PurgeScope.ForApp("signal.exe"), "site" => PurgeScope.ForSite("chat.example.org"), _ => PurgeScope.ForId(id) };
        Assert.Equal(1, S.Purge(scope));
        var n = Assert.Single(_seen);
        Assert.Equal([id], n.Ids);
        Assert.Null(n.From);
        Assert.NotNull(n.Newest);
    }

    [Fact]
    public void Purge_that_matches_nothing_tells_nothing()
    {
        S.Purge(PurgeScope.ForId("s_nothing"));
        Assert.Empty(_seen);
    }

    [Fact]
    public void Retention_space_drop_and_replay_deletes_tell_their_reason()
    {
        var now = DateTimeOffset.UtcNow;
        var expired = S.AddScreenCapture(Caps.Screen(now.AddDays(-40)));
        var settings = new Settings();
        settings.Retention.Days = 30;
        S.RunMaintenance(settings);
        Assert.Contains(_seen, n => n.Reason == "retention" && n.Ids.Contains(expired) && !n.ByUser);

        var oldest = S.AddScreenCapture(Caps.Screen(now.AddDays(-2)));
        Assert.Equal(1, S.DeleteForSpace(1));
        Assert.Contains(_seen, n => n.Reason == "space" && n.Ids.SequenceEqual([oldest]));

        var nsfw = S.AddScreenCapture(Caps.Screen(now.AddSeconds(-3)));
        S.DropCapture(nsfw, "nsfw");
        Assert.Contains(_seen, n => n.Reason == "drop" && n.Ids.SequenceEqual([nsfw]));

        var frame = S.AddReplayFrame(new NewReplayFrame(now.AddSeconds(-2), "rp1", 0, "app", "t", 0, 64, 32, Caps.FakeWebp, "image/webp"));
        Assert.Equal(1, S.DeleteReplayFrames("rp1"));
        Assert.Contains(_seen, n => n.Reason == "replay" && n.Ids.SequenceEqual([frame]));
        Assert.DoesNotContain(_seen, n => n.ByUser);
    }

    [Fact]
    public void Listener_failure_does_not_break_the_purge_or_later_listeners()
    {
        var later = new List<PurgeNotice>();
        S.Purged += _ => throw new InvalidOperationException("boom");
        S.Purged += later.Add;
        S.AddScreenCapture(Caps.Screen(DateTimeOffset.UtcNow.AddSeconds(-5)));
        Assert.Equal(1, S.Purge(PurgeScope.Last(TimeSpan.FromMinutes(1))));
        Assert.Single(later);
    }

    // ---------------------------------------------------------------- fence (B12, N4)

    [Fact]
    public void Range_purge_ending_now_fences_its_window_only()
    {
        // Danger zone "today": purge range <start of day> <now>. A frame still in flight from inside the window is refused,
        // one stamped before the window and one taken after the purge are kept.
        var now = DateTimeOffset.UtcNow;
        S.Purge(PurgeScope.Range(now.AddHours(-1), now));
        Assert.Throws<PurgedWindowException>(() => S.AddScreenCapture(Caps.Screen(now.AddSeconds(-10))));
        Assert.Throws<PurgedWindowException>(() => S.AddAudioCapture(Caps.Audio(now.AddMinutes(-30), "late transcript")));
        Assert.NotNull(S.AddScreenCapture(Caps.Screen(now.AddHours(-2))));
        Assert.NotNull(S.AddScreenCapture(Caps.Screen(DateTimeOffset.UtcNow.AddSeconds(1))));
    }

    [Fact]
    public void Range_purge_ending_in_the_future_fences_up_to_now()
    {
        var now = DateTimeOffset.UtcNow;
        S.Purge(PurgeScope.Range(now.AddHours(-1), now.AddHours(5)));
        Assert.Throws<PurgedWindowException>(() => S.AddScreenCapture(Caps.Screen(now.AddSeconds(-1))));
        Assert.NotNull(S.AddScreenCapture(Caps.Screen(DateTimeOffset.UtcNow.AddSeconds(2))));   // new captures go on
    }

    [Fact]
    public void Old_range_purge_sets_no_fence()
    {
        var now = DateTimeOffset.UtcNow;
        S.Purge(PurgeScope.Range(now.AddDays(-3), now.AddDays(-2)));
        Assert.NotNull(S.AddScreenCapture(Caps.Screen(now.AddDays(-2).AddHours(-1))));
    }

    [Fact]
    public void Fence_expires_by_the_monotonic_clock_after_a_wall_clock_step_back()
    {
        // The wall clock was ahead at the purge and NTP set it back: new captures are stamped inside the fenced window.
        // The fence lasts FenceLifetime by the monotonic clock, not until the wall clock catches up.
        long mono = 1_000_000;
        S.MonoMs = () => mono;
        var now = DateTimeOffset.UtcNow;
        S.Purge(PurgeScope.Last(TimeSpan.FromMinutes(10)));
        var stepped = now.AddMinutes(-3);   // "now" after the step back
        Assert.Throws<PurgedWindowException>(() => S.AddScreenCapture(Caps.Screen(stepped)));
        mono += (long)EncryptedStore.FenceLifetime.TotalMilliseconds;
        Assert.NotNull(S.AddScreenCapture(Caps.Screen(stepped)));
    }

    [Fact]
    public void Replay_carry_in_sample_restamped_after_the_fence_is_refused_by_its_grab_time()
    {
        // N4: the newest sample before a clip start is re-stamped to the start; its pixels are from before the purge.
        var now = DateTimeOffset.UtcNow;
        S.Purge(PurgeScope.Last(TimeSpan.FromMinutes(5)));
        var frame = new NewReplayFrame(DateTimeOffset.UtcNow.AddSeconds(1), "rp2", 0, "app", "t", 0, 64, 32, Caps.FakeWebp, "image/webp",
            CapturedAt: now.AddSeconds(-20));
        Assert.Throws<PurgedWindowException>(() => S.AddReplayFrame(frame));
        Assert.NotNull(S.AddReplayFrame(frame with { CapturedAt = null }));
    }
}

/// <summary>CoreHost forwards every store's purge notice, and purge all, as ICoreHost.Purged (N5).</summary>
public sealed class CoreHostPurgedTests
{
    [Fact]
    public async Task Store_purges_and_purge_all_reach_core_host_listeners()
    {
        using var h = new TempHome("dpapi");
        var host = (CoreHostImpl)CoreHost.Create(new CoreHostOptions(h.Path, h.Path));
        await using (host)
        {
            var seen = new List<PurgeNotice>();
            ((ICoreHost)host).Purged += seen.Add;
            await host.OpenStoreAsync(CancellationToken.None);
            var id = host.Store!.AddScreenCapture(Caps.Screen(DateTimeOffset.UtcNow.AddSeconds(-5)));
            host.Store.Purge(PurgeScope.ForId(id));
            Assert.Contains(seen, n => n.Reason == "purge" && n.Ids.Contains(id));

            await host.PurgeAllAsync(CancellationToken.None);
            var all = Assert.Single(seen, n => n.Reason == "all");
            Assert.True(all.ByUser);
            Assert.NotNull(all.To);

            // The store opened after purge all is wired too.
            for (int i = 0; i < 50 && host.Store is null; i++) await Task.Delay(50);
            var id2 = host.Store!.AddScreenCapture(Caps.Screen(DateTimeOffset.UtcNow.AddSeconds(1)));
            host.Store.Purge(PurgeScope.ForId(id2));
            Assert.Contains(seen, n => n.Ids.Contains(id2));
        }
    }
}
