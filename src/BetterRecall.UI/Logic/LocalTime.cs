// SPDX-License-Identifier: MIT
// Copyright (c) BetterRecall contributors
namespace BetterRecall.UI.Logic;

public static class LocalTime
{
    /// <summary>A local wall-clock time with the UTC offset in force on that date, not today's: across a daylight-saving
    /// change today's offset is an hour off. An ambiguous time (clocks set back) takes standard time, a skipped one
    /// (clocks set forward) moves forward an hour.</summary>
    public static DateTimeOffset At(DateTime local, TimeZoneInfo? zone = null)
    {
        zone ??= TimeZoneInfo.Local;
        var t = DateTime.SpecifyKind(local, DateTimeKind.Unspecified);
        if (zone.IsInvalidTime(t)) t = t.AddHours(1);
        return new DateTimeOffset(t, zone.GetUtcOffset(t));
    }
}
