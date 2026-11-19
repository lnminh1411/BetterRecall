// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Globalization;
using System.Text;

namespace BetterRecall.Worker.Hosting;

/// <summary>Rolling daily log in &lt;home&gt;\logs\worker-YYYYMMDD.log. Never logs captured content, titles, query or
/// transcript text: callers pass only op names, sizes, timings, device names and error types. One file per day, 10 MB
/// cap, 14 files kept.</summary>
public static class Log
{
    internal const long MaxFileBytes = 10 * 1024 * 1024;
    internal const int KeepFiles = 14;
    private static readonly LogSink Sink = new();

    public static void Init(string? logsDir, string level) => Sink.Init(logsDir, level);

    public static void Debug(string m) => Sink.Write(0, "DBG", m);
    public static void Info(string m) => Sink.Write(1, "INF", m);
    public static void Warn(string m) => Sink.Write(2, "WRN", m);
    public static void Error(string m) => Sink.Write(3, "ERR", m);
}

internal sealed class LogSink(Func<DateTimeOffset>? clock = null)
{
    private static readonly UTF8Encoding Utf8NoBom = new(encoderShouldEmitUTF8Identifier: false);
    private readonly Func<DateTimeOffset> _now = clock ?? (() => DateTimeOffset.Now);
    private readonly Lock _gate = new();
    private volatile string? _dir;
    private volatile int _level = 1; // 0 debug, 1 info, 2 warn, 3 error
    private string? _currentPath;
    private string? _currentDay;

    public void Init(string? logsDir, string level)
    {
        lock (_gate)
        {
            _level = level switch { "debug" => 0, "warn" => 2, "error" => 3, _ => 1 };
            _currentPath = null;
            _currentDay = null;
            if (logsDir is null) { _dir = null; return; }
            try
            {
                var fullDir = Path.GetFullPath(logsDir);
                Directory.CreateDirectory(fullDir);
                _dir = fullDir;
                PruneLocked();
            }
            catch { _dir = null; }
        }
    }

    public void Write(int lvl, string tag, string m)
    {
        if (lvl < _level || _dir is null) return;
        var now = _now();
        var line = $"{now.ToString("yyyy-MM-ddTHH:mm:ss.fffzzz", CultureInfo.InvariantCulture)} {tag} [{Environment.ProcessId}] {m}{Environment.NewLine}";
        lock (_gate)
        {
            try
            {
                if (_dir is null) return;
                var day = now.ToString("yyyyMMdd", CultureInfo.InvariantCulture);
                bool needPick = _currentPath is null || _currentDay != day || IsOverLimit(_currentPath);
                if (needPick)
                {
                    _currentPath = PickPathLocked(day, now);
                    _currentDay = day;
                }
                File.AppendAllText(_currentPath!, line, Utf8NoBom);
                if (needPick) PruneLocked();
            }
            catch { /* logging never fails the worker */ }
        }
    }

    private static bool IsOverLimit(string path)
    {
        var fi = new FileInfo(path);
        return fi.Exists && fi.Length >= Log.MaxFileBytes;
    }

    private string PickPathLocked(string day, DateTimeOffset now)
    {
        var basePath = Path.Combine(_dir!, $"worker-{day}.log");
        if (!IsOverLimit(basePath)) return basePath;
        var stamp = now.ToString("HHmmss", CultureInfo.InvariantCulture);
        var candidate = Path.Combine(_dir!, $"worker-{day}-{stamp}.log");
        if (!IsOverLimit(candidate)) return candidate;
        for (int i = 1; ; i++)
        {
            candidate = Path.Combine(_dir!, $"worker-{day}-{stamp}-{i}.log");
            if (!IsOverLimit(candidate)) return candidate;
        }
    }

    private void PruneLocked()
    {
        if (_dir is null) return;
        var currentName = _currentPath is null ? null : Path.GetFileName(_currentPath);
        var files = new DirectoryInfo(_dir).GetFiles("worker-*.log")
            .OrderByDescending(f => currentName is not null && string.Equals(f.Name, currentName, StringComparison.OrdinalIgnoreCase))
            .ThenByDescending(f => f.LastWriteTimeUtc)
            .ThenByDescending(f => f.Name, StringComparer.Ordinal);
        foreach (var f in files.Skip(Log.KeepFiles))
        {
            try { f.Delete(); } catch { }
        }
    }
}
