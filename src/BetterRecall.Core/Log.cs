// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Text;

namespace BetterRecall.Core;

public enum LogLevel { Debug = 0, Info = 1, Warn = 2, Error = 3 }

/// <summary>
/// Rolling file log in &lt;home&gt;\logs. Callers must never pass captured content: no titles, queries,
/// transcripts, paths of captured windows or key material. One file per day per process, 10 MB cap, 14 files kept.
/// </summary>
public static class Log
{
    private const long MaxFileBytes = 10 * 1024 * 1024;
    private const int KeepFiles = 14;

    private static readonly Lock Gate = new();
    private static string? _dir;
    private static string _prefix = "core";
    private static LogLevel _level = LogLevel.Info;
    private static StreamWriter? _writer;
    private static string? _currentPath;

    public static LogLevel Level { get => _level; set => _level = value; }

    public static bool Initialized { get { lock (Gate) return _dir != null; } }

    /// <summary>Starts file logging. Safe to call more than once; the last call wins.</summary>
    public static void Init(string logsDir, string prefix)
    {
        lock (Gate)
        {
            _dir = logsDir;
            _prefix = prefix;
            CloseWriter();
            try { Directory.CreateDirectory(logsDir); Prune(); } catch { /* logging must never throw */ }
        }
    }

    public static LogLevel ParseLevel(string? s) => s?.ToLowerInvariant() switch
    {
        "debug" => LogLevel.Debug,
        "warn" => LogLevel.Warn,
        "error" => LogLevel.Error,
        _ => LogLevel.Info,
    };

    public static void Debug(string msg) => Write(LogLevel.Debug, msg, null);
    public static void Info(string msg) => Write(LogLevel.Info, msg, null);
    public static void Warn(string msg, Exception? ex = null) => Write(LogLevel.Warn, msg, ex);
    public static void Error(string msg, Exception? ex = null) => Write(LogLevel.Error, msg, ex);

    private static void Write(LogLevel level, string msg, Exception? ex)
    {
        if (level < _level) return;
        var sb = new StringBuilder(128);
        sb.Append(DateTime.UtcNow.ToString("yyyy-MM-ddTHH:mm:ss.fffZ")).Append(' ')
          .Append(level switch { LogLevel.Debug => "DBG", LogLevel.Info => "INF", LogLevel.Warn => "WRN", _ => "ERR" })
          .Append(' ').Append(Environment.CurrentManagedThreadId).Append(' ').Append(msg);
        if (ex != null)
        {
            // Exception type and stack only. Messages can carry SQL text or paths, so they are kept out.
            sb.Append(" | ").Append(ex.GetType().FullName).Append(" HResult=0x").Append(ex.HResult.ToString("X8"));
            if (ex.StackTrace != null) sb.Append(Environment.NewLine).Append(ex.StackTrace);
            if (ex.InnerException != null) sb.Append(" | inner ").Append(ex.InnerException.GetType().FullName);
        }
        lock (Gate)
        {
            try
            {
                if (_dir == null) return;
                var path = Path.Combine(_dir, $"{_prefix}-{DateTime.UtcNow:yyyyMMdd}.log");
                if (_writer == null || _currentPath != path || _writer.BaseStream.Length > MaxFileBytes)
                {
                    CloseWriter();
                    if (File.Exists(path) && new FileInfo(path).Length > MaxFileBytes)
                        path = Path.Combine(_dir, $"{_prefix}-{DateTime.UtcNow:yyyyMMdd-HHmmss}.log");
                    _writer = new StreamWriter(new FileStream(path, FileMode.Append, FileAccess.Write, FileShare.ReadWrite | FileShare.Delete), new UTF8Encoding(false)) { AutoFlush = true };
                    _currentPath = path;
                    Prune();
                }
                _writer.WriteLine(sb.ToString());
            }
            catch { /* logging must never throw */ }
        }
    }

    private static void CloseWriter()
    {
        try { _writer?.Dispose(); } catch { }
        _writer = null;
        _currentPath = null;
    }

    private static void Prune()
    {
        if (_dir == null) return;
        var files = new DirectoryInfo(_dir).GetFiles(_prefix + "-*.log").OrderByDescending(f => f.LastWriteTimeUtc).ToList();
        foreach (var f in files.Skip(KeepFiles)) { try { f.Delete(); } catch { } }
    }
}
