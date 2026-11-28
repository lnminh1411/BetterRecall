// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using BetterRecall.UI.Logic;

namespace BetterRecall.UI.Tests;

public sealed class LocalTimeTests
{
    [Fact]
    public void Local_time_uses_the_offset_of_that_date_and_advances_skipped_times()
    {
        var cet = TimeZoneInfo.FindSystemTimeZoneById("Central European Standard Time");
        Assert.Equal(TimeSpan.FromHours(2), LocalTime.At(new DateTime(2026, 10, 20, 0, 0, 0), cet).Offset);   // summer time
        Assert.Equal(TimeSpan.FromHours(1), LocalTime.At(new DateTime(2026, 11, 13, 0, 0, 0), cet).Offset);
        Assert.Equal(new DateTime(2026, 3, 29, 3, 30, 0), LocalTime.At(new DateTime(2026, 3, 29, 2, 30, 0), cet).DateTime);   // skipped hour

        // A zone whose clocks jump 30 minutes at 02:00 (like Lord Howe Island).
        var rule = TimeZoneInfo.AdjustmentRule.CreateAdjustmentRule(new DateTime(2000, 1, 1), new DateTime(2099, 12, 31), TimeSpan.FromMinutes(30),
            TimeZoneInfo.TransitionTime.CreateFloatingDateRule(new DateTime(1, 1, 1, 2, 0, 0), 10, 1, DayOfWeek.Sunday),
            TimeZoneInfo.TransitionTime.CreateFloatingDateRule(new DateTime(1, 1, 1, 2, 0, 0), 4, 1, DayOfWeek.Sunday));
        var zone = TimeZoneInfo.CreateCustomTimeZone("Test Half Hour", TimeSpan.FromHours(10.5), "Test", "Test", "Test DST", [rule]);
        var skipped = new DateTime(2026, 10, 4, 2, 10, 0);
        Assert.True(zone.IsInvalidTime(skipped));
        var at = LocalTime.At(skipped, zone);
        Assert.Equal(new DateTime(2026, 10, 4, 2, 40, 0), at.DateTime);
        Assert.Equal(TimeSpan.FromHours(11), at.Offset);
    }

    /// <summary>C17: Today uses the offset at midnight even on a daylight saving transition day.</summary>
    [Fact]
    public void Today_starts_at_local_midnight_on_a_daylight_saving_day()
    {
        var cet = TimeZoneInfo.FindSystemTimeZoneById("Central European Standard Time");
        // 2026-03-29: clocks went forward at 02:00; at 10:00 the offset is +2, at midnight it was +1.
        var now = new DateTimeOffset(2026, 3, 29, 10, 0, 0, TimeSpan.FromHours(2));
        var (from, to) = SearchViewModel.RangeBounds(TimeRange.Today, now, cet);
        Assert.Null(to);
        Assert.Equal(new DateTimeOffset(2026, 3, 29, 0, 0, 0, TimeSpan.FromHours(1)), from);
        Assert.Equal(new DateTimeOffset(2026, 3, 28, 23, 0, 0, TimeSpan.Zero), from!.Value.ToUniversalTime());
        Assert.Equal((null, null), SearchViewModel.RangeBounds(TimeRange.Any, now, cet));
    }
}
