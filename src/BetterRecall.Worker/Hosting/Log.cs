// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Text;

namespace BetterRecall.Worker.Hosting;

/// <summary>Rolling daily log in &lt;home&gt;\logs\worker-YYYYMMDD.log. Never logs captured content, titles, query or
/// transcript text: callers pass only op names, sizes, timings, device names and error types.</summary>
public static class Log
{
    private static readonly object Gate = new();
    private static string? _dir;
    private static int _level = 1; // 0 debug, 1 info, 2 warn, 3 error

    public static void Init(string? logsDir, string level)
    {
        _level = level switch { "debug" => 0, "warn" => 2, "error" => 3, _ => 1 };
        if (logsDir is null) { _dir = null; return; }
        try { Directory.CreateDirectory(logsDir); _dir = logsDir; } catch { _dir = null; }
    }

    public static void Debug(string m) => Write(0, "DBG", m);
    public static void Info(string m) => Write(1, "INF", m);
    public static void Warn(string m) => Write(2, "WRN", m);
    public static void Error(string m) => Write(3, "ERR", m);

    private static void Write(int lvl, string tag, string m)
    {
        if (lvl < _level || _dir is null) return;
        var now = DateTimeOffset.Now;
        var line = $"{now:yyyy-MM-ddTHH:mm:ss.fffzzz} {tag} [{Environment.ProcessId}] {m}{Environment.NewLine}";
        lock (Gate)
        {
            try { File.AppendAllText(Path.Combine(_dir, $"worker-{now:yyyyMMdd}.log"), line, Encoding.UTF8); }
            catch { /* logging never fails the worker */ }
        }
    }
}
