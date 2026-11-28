// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

namespace BetterRecall.UI.Logic;

/// <summary>Settings page layout (sweep 3 D15). Below this page width the main nav is already an icon rail (or a
/// hamburger), so the group list moves to a labelled top strip instead of a second icon rail.</summary>
public static class SettingsShellLayout
{
    public const double TopBelow = 860;

    public static bool GroupsOnTop(double pageWidth) => pageWidth > 0 && pageWidth < TopBelow;
}

/// <summary>Audit log columns (sweep 3 D14): under this page width the Results and PID columns are hidden, so the
/// Client column keeps its room instead of clipping.</summary>
public static class AuditLayout
{
    public const double NarrowBelow = 700;

    public static bool Narrow(double pageWidth) => pageWidth > 0 && pageWidth < NarrowBelow;

    /// <summary>Widths of the six columns (time, pipe, verb, results, pid, client); client is star-sized.</summary>
    public static double[] FixedWidths(bool narrow) => narrow ? [150, 44, 120, 0, 0] : [150, 44, 120, 52, 60];
}
