// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
namespace BetterRecall.UI.Logic;

public static class LocalTime
{
    /// <summary>A local wall-clock time with the UTC offset in force on that date, not today's: across a daylight-saving
    /// change today's offset is an hour off. An ambiguous time (clocks set back) takes standard time, a skipped one
    /// (clocks set forward) moves forward by the size of the gap (an hour in most zones, 30 minutes in some).</summary>
    public static DateTimeOffset At(DateTime local, TimeZoneInfo? zone = null)
    {
        zone ??= TimeZoneInfo.Local;
        var t = DateTime.SpecifyKind(local, DateTimeKind.Unspecified);
        if (!zone.IsInvalidTime(t)) return new DateTimeOffset(t, zone.GetUtcOffset(t));
        // Skipped: read the wall time with the offset in force just before the gap, then show that instant in the zone.
        var before = t;
        for (int i = 0; i < 24 * 60 && zone.IsInvalidTime(before); i++) before = before.AddMinutes(-1);
        var utc = new DateTimeOffset(t, zone.GetUtcOffset(before));
        return TimeZoneInfo.ConvertTime(utc, zone);
    }
}
