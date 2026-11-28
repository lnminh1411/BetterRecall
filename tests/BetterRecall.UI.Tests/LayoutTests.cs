// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using BetterRecall.UI.Logic;

namespace BetterRecall.UI.Tests;

/// <summary>Responsive narrow-width layout thresholds (D14, D15).</summary>
public sealed class LayoutTests
{
    [Fact]
    public void Settings_groups_move_to_the_top_when_narrow()
    {
        Assert.True(SettingsShellLayout.GroupsOnTop(852));    // 900 px window less the main icon rail
        Assert.True(SettingsShellLayout.GroupsOnTop(640));
        Assert.False(SettingsShellLayout.GroupsOnTop(1152));
        Assert.False(SettingsShellLayout.GroupsOnTop(0));     // before the first layout pass
    }

    [Fact]
    public void Audit_hides_results_and_pid_when_narrow()
    {
        Assert.True(AuditLayout.Narrow(564));
        Assert.False(AuditLayout.Narrow(900));
        var narrow = AuditLayout.FixedWidths(true);
        Assert.Equal(0, narrow[3]);
        Assert.Equal(0, narrow[4]);
        // time, pipe, verb, the 120 px client minimum and five 12 px gaps fit a 640 px window's page
        Assert.True(narrow.Sum() + 120 + 5 * 12 <= 564);
        Assert.Equal([150, 44, 120, 52, 60], AuditLayout.FixedWidths(false));
    }
}
