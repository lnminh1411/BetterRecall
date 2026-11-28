// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Globalization;
using System.Text;

namespace BetterRecall.UI.Logic;

/// <summary>Exceptions the UI swallows (App.UnhandledException) go to logs\ui-errors.log: type names and stack frames
/// only, never the message, which can quote captured text. The file is started over past MaxBytes.</summary>
public static class UiErrorLog
{
    public const string FileName = "ui-errors.log";
    public const long MaxBytes = 256 * 1024;

    /// <summary>One entry: time, then each exception's type (inner ones too) and its stack trace.</summary>
    public static string Format(Exception? ex, DateTimeOffset now)
    {
        var sb = new StringBuilder();
        sb.Append(now.ToString("yyyy-MM-dd HH:mm:ss.fff zzz", CultureInfo.InvariantCulture)).Append(" unhandled\n");
        for (int depth = 0; ex is not null && depth < 8; ex = ex.InnerException, depth++)
        {
            sb.Append(depth == 0 ? "  " : "  inner ").Append(ex.GetType().FullName).Append(" (0x").Append(ex.HResult.ToString("X8", CultureInfo.InvariantCulture)).Append(")\n");
            if (ex.StackTrace is { Length: > 0 } st)
                foreach (var line in st.Split('\n')) sb.Append("    ").Append(line.TrimEnd('\r').Trim()).Append('\n');
        }
        return sb.ToString();
    }

    /// <summary>Appends one entry. Never throws: logging must not turn a handled error into a crash.</summary>
    public static void Append(string dir, Exception? ex)
    {
        try
        {
            Directory.CreateDirectory(dir);
            var path = Path.Combine(dir, FileName);
            var entry = Format(ex, DateTimeOffset.Now);
            bool fresh = new FileInfo(path) is { Exists: true, Length: > MaxBytes };
            using var fs = new FileStream(path, fresh ? FileMode.Create : FileMode.Append, FileAccess.Write, FileShare.ReadWrite);
            fs.Write(Encoding.UTF8.GetBytes(entry));
        }
        catch { }
    }
}
