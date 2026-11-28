// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Diagnostics;

namespace BetterRecall.UI.Logic;

/// <summary>The offline bar's Start action (sweep 3 D10): starts the installed agent next to this UI. The agent is
/// single instance per data folder, so a second start while one runs exits at once.</summary>
public static class AgentStart
{
    /// <summary>The agent exe of the installed layout, or null when it is not there (dev runs, mock).</summary>
    public static string? AgentPath(string uiBaseDirectory)
    {
        var path = ServerTrustVerifier.InstalledAgentPath(uiBaseDirectory);
        return File.Exists(path) ? path : null;
    }

    /// <summary>False when there is no agent to start or Windows refused to start it.</summary>
    public static bool TryStart(string uiBaseDirectory)
    {
        if (AgentPath(uiBaseDirectory) is not { } path) return false;
        try
        {
            using var p = Process.Start(new ProcessStartInfo(path) { UseShellExecute = false, WorkingDirectory = Path.GetDirectoryName(path)! });
            return p is not null;
        }
        catch (Exception ex) when (ex is System.ComponentModel.Win32Exception or InvalidOperationException or IOException)
        {
            return false;
        }
    }
}
