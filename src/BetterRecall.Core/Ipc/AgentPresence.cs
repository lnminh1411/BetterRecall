// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using BetterRecall.Contracts;

namespace BetterRecall.Core.Ipc;

/// <summary>Why the public pipe did not answer (sweep 3 D8): the agent of this data folder holds its single-instance
/// mutex while it runs, so a held mutex with no pipe means scripts (IPC) are off, not that BetterRecall is down.</summary>
public static class AgentPresence
{
    public const string ErrNotRunning = "err BetterRecall not running";
    public const string ErrIpcOff = "err Scripts (IPC) are off. Turn on Settings > IPC and MCP > Allow scripts (IPC).";

    /// <summary>True when an agent holds the mutex of this data folder.</summary>
    public static bool Running(string home)
    {
        try
        {
            if (!Mutex.TryOpenExisting(PipeNames.AgentMutexFor(home), out var m)) return false;
            m.Dispose();
            return true;
        }
        catch (UnauthorizedAccessException) { return true; }   // it exists; only this token may not open it
    }

    /// <summary>The CLI line for a pipe that did not answer, and its exit code: 3 when the agent runs and settings have
    /// ipc.enabled off, else 2 (not running, or running with IPC on but not answering in time).</summary>
    public static (string Line, int Exit) NotAnswering(string home, bool ipcEnabled) =>
        !ipcEnabled && Running(home) ? (ErrIpcOff, 3) : (ErrNotRunning, 2);
}
