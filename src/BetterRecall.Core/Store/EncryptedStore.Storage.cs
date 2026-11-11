// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Numerics;
using BetterRecall.Contracts;
using Microsoft.Data.Sqlite;

namespace BetterRecall.Core.Store;

/// <summary>
/// Storage policy (0.2 batch 2): change score and window/app switch flag per screen row, progressive thinning
/// (retention.progressiveAfterDays / progressiveOverDays), deletion for space (storage.overwrite) and the
/// live/free byte counts the agent's storage guard reads.
/// </summary>
public sealed partial class EncryptedStore
{
    private const long DayMs = 86_400_000;

    /// <summary>One screen row of a day with its effective change score and switch flag.</summary>
    internal readonly record struct ScoredRow(long Rid, long Ts, double Change, bool Switch);

    /// <summary>
    /// change_pct (REAL, null on rows from before 0.2 batch 2), is_switch (null likewise) and thin_days
    /// (row count of a day when it entered thinning). Idempotent.
    /// </summary>
    private void MigrateStorage(HashSet<string> cols)
    {
        if (!cols.Contains("change_pct")) Exec(_w, "ALTER TABLE captures ADD COLUMN change_pct REAL");
        if (!cols.Contains("is_switch")) Exec(_w, "ALTER TABLE captures ADD COLUMN is_switch INTEGER");
        Exec(_w, "CREATE TABLE IF NOT EXISTS thin_days(day INTEGER PRIMARY KEY, total INTEGER NOT NULL)");
    }

    /// <summary>
    /// Screen rows in [fromMs, toMs) with their scores. Rows written before the change score existed get one from
    /// the dHash distance to the previous row of the same monitor, and count as a switch when app or title differ
    /// (or when there is no previous row in the window). Caller holds _wl.
    /// </summary>
    private List<ScoredRow> ScreenRowsLocked(long fromMs, long toMs)
    {
        var rows = new List<ScoredRow>();
        using var cmd = Cmd(_w, """
            SELECT rid, ts, monitor, app, title, dhash, change_pct, is_switch FROM captures
            WHERE kind=0 AND ts >= $f AND ts < $t ORDER BY monitor, ts
            """, ("$f", fromMs), ("$t", toMs));
        using var rd = cmd.ExecuteReader();
        int prevMonitor = int.MinValue;
        string prevApp = "", prevTitle = "";
        long prevHash = 0;
        while (rd.Read())
        {
            long rid = rd.GetInt64(0), ts = rd.GetInt64(1);
            int monitor = rd.GetInt32(2);
            string app = rd.GetString(3), title = rd.GetString(4);
            long hash = rd.GetInt64(5);
            double? change = rd.IsDBNull(6) ? null : rd.GetDouble(6);
            int? sw = rd.IsDBNull(7) ? null : rd.GetInt32(7);
            bool first = monitor != prevMonitor;
            bool isSwitch = sw is int s ? s == 1 : first || app != prevApp || title != prevTitle;
            double score = change ?? (first ? 100 : BitOperations.PopCount((ulong)(prevHash ^ hash)) * 100.0 / 64);
            rows.Add(new ScoredRow(rid, ts, score, isSwitch));
            prevMonitor = monitor;
            prevApp = app;
            prevTitle = title;
            prevHash = hash;
        }
        return rows;
    }

    /// <summary>
    /// How many non-switch rows of a day survive at fraction p of the thinning period (0 = all, 1 = none).
    /// total is the day's count when it entered thinning, so the curve does not restart after each pass.
    /// </summary>
    internal static int ThinTarget(int total, double p)
    {
        if (p <= 0) return total;
        if (p >= 1) return 0;
        return (int)Math.Ceiling(total * (1 - p));
    }

    /// <summary>
    /// Progressive thinning: from progressiveAfterDays, each day keeps a share of its non-switch screen rows that
    /// falls linearly to zero over progressiveOverDays, dropping the smallest changes first. Window/app switches and
    /// audio stay until retention.days. Caller holds _wl. Returns rows deleted.
    /// </summary>
    private int ThinLocked(RetentionSettings r, DateTimeOffset now, long cutoffMs)
    {
        long nowMs = now.ToUnixTimeMilliseconds();
        long cutoffDay = Math.DivRem(cutoffMs, DayMs, out _);
        Exec(_w, "DELETE FROM thin_days WHERE day < $d", ("$d", cutoffDay));
        if (r.ProgressiveAfterDays <= 0) return 0;
        int over = Math.Max(1, r.ProgressiveOverDays);
        long lastDay = (nowMs - (long)r.ProgressiveAfterDays * DayMs) / DayMs;
        int deleted = 0;
        for (long day = cutoffDay; day <= lastDay; day++)
        {
            long start = day * DayMs;
            double ageDays = (nowMs - (start + DayMs / 2)) / (double)DayMs;
            double p = (ageDays - r.ProgressiveAfterDays) / over;
            if (p <= 0) continue;
            var candidates = ScreenRowsLocked(Math.Max(start, cutoffMs), start + DayMs).Where(x => !x.Switch).ToList();
            var total = Scalar(_w, "SELECT total FROM thin_days WHERE day=$d", ("$d", day)) as long?;
            if (total is null)
            {
                if (candidates.Count == 0) continue;
                total = candidates.Count;
                Exec(_w, "INSERT INTO thin_days(day, total) VALUES($d, $n)", ("$d", day), ("$n", total.Value));
            }
            int drop = candidates.Count - ThinTarget((int)Math.Min(int.MaxValue, total.Value), p);
            if (drop <= 0) continue;
            var rids = candidates.OrderBy(x => x.Change).ThenBy(x => x.Ts).Take(drop).Select(x => x.Rid).ToList();
            deleted += DeleteRowsLocked(rids);
        }
        return deleted;
    }

    /// <summary>
    /// Rows in progressive order for the oldest day that has any: non-switch screen rows by smallest change, then
    /// window/app switches and audio by age. Caller holds _wl.
    /// </summary>
    private List<long> SpaceOrderLocked(int max)
    {
        if (Scalar(_w, "SELECT min(ts) FROM captures") is not long oldest) return [];
        long start = oldest / DayMs * DayMs;
        var screen = ScreenRowsLocked(start, start + DayMs);
        var order = screen.Where(x => !x.Switch).OrderBy(x => x.Change).ThenBy(x => x.Ts).Select(x => (x.Rid, x.Ts)).ToList();
        var rest = screen.Where(x => x.Switch).Select(x => (x.Rid, x.Ts)).ToList();
        using (var cmd = Cmd(_w, "SELECT rid, ts FROM captures WHERE kind<>0 AND ts >= $f AND ts < $t", ("$f", start), ("$t", start + DayMs)))
        using (var rd = cmd.ExecuteReader())
            while (rd.Read()) rest.Add((rd.GetInt64(0), rd.GetInt64(1)));
        order.AddRange(rest.OrderBy(x => x.Ts));
        return order.Take(max).Select(x => x.Rid).ToList();
    }

    public int DeleteForSpace(int maxRows)
    {
        if (maxRows <= 0) return 0;
        lock (_wl)
        {
            ThrowIfDisposed();
            int total = 0;
            // The oldest day may hold fewer rows than asked for: continue with the next one.
            while (total < maxRows)
            {
                var rids = SpaceOrderLocked(maxRows - total);
                if (rids.Count == 0) break;
                total += DeleteRowsLocked(rids);
            }
            if (total > 0)
            {
                DropEmptyMonthFilesLocked();
                ReleaseFreePagesLocked(vacuumLegacy: false);
                Log.Info($"storage: {total} oldest rows deleted for space");
            }
            return total;
        }
    }

    /// <summary>Deletes keyframe files whose month has no keyframe left (frees the disk at once). Caller holds _wl.</summary>
    private void DropEmptyMonthFilesLocked()
    {
        foreach (var m in KfMonths())
        {
            if ((long)(Scalar(_w, "SELECT count(*) FROM captures WHERE kf_month=$m AND has_keyframe=1", ("$m", m)) ?? 0L) != 0) continue;
            // Rows of that month that are still pending can no longer be embedded.
            Exec(_w, "UPDATE captures SET state=2 WHERE kf_month=$m AND state=0", ("$m", m));
            Exec(_w, "DELETE FROM jobs WHERE rid IN (SELECT rid FROM captures WHERE kf_month=$m AND kind=0)", ("$m", m));
            DeleteKfFile(m);
        }
    }

    /// <summary>
    /// Gives free pages back to the disk: incremental_vacuum on month files created with auto_vacuum=INCREMENTAL;
    /// with vacuumLegacy a full VACUUM of older month files whose free pages pass a quarter of the file and 64 MiB
    /// (needs about the file size in temporary space, so the hourly maintenance does it, not the space guard).
    /// Caller holds _wl.
    /// </summary>
    private void ReleaseFreePagesLocked(bool vacuumLegacy = true)
    {
        foreach (var m in KfMonths())
        {
            if (Kf(m, false) is not { } c) continue;
            try
            {
                long free = (long)(Scalar(c, "PRAGMA freelist_count") ?? 0L);
                if (free == 0) continue;
                long mode = (long)(Scalar(c, "PRAGMA auto_vacuum") ?? 0L);
                if (mode == 2)
                {
                    Exec(c, "PRAGMA incremental_vacuum");
                    Exec(c, "PRAGMA wal_checkpoint(TRUNCATE)");
                    continue;
                }
                if (!vacuumLegacy) continue;
                long pages = (long)(Scalar(c, "PRAGMA page_count") ?? 0L);
                long pageSize = (long)(Scalar(c, "PRAGMA page_size") ?? 4096L);
                if (pages > 0 && free * 4 > pages && free * pageSize > 64L * 1024 * 1024)
                {
                    Exec(c, "PRAGMA wal_checkpoint(TRUNCATE)");
                    Exec(c, "VACUUM");
                    Log.Info("keyframe file vacuumed");
                }
            }
            catch (SqliteException ex) { Log.Warn("keyframe file compaction failed", ex); }
        }
    }

    public StorageUse GetStorageUse()
    {
        lock (_wl)
        {
            ThrowIfDisposed();
            var db = DataPaths.StoreDb(_home);
            long files = FileSize(db) + FileSize(db + "-wal");
            long free = FreeBytes(_w);
            foreach (var m in KfMonths())
            {
                var p = KfPath(m);
                files += FileSize(p) + FileSize(p + "-wal");
                if (Kf(m, false) is { } c) free += FreeBytes(c);
            }
            return new StorageUse(files, Math.Min(free, files));
        }
    }

    private static long FreeBytes(SqliteConnection c)
    {
        try { return (long)(Scalar(c, "PRAGMA freelist_count") ?? 0L) * (long)(Scalar(c, "PRAGMA page_size") ?? 4096L); }
        catch (SqliteException) { return 0; }
    }

    /// <summary>Tests: change score and switch flag as stored (null when unknown).</summary>
    internal (double? Change, bool? Switch) StoredScore(string id)
    {
        lock (_wl)
        {
            using var cmd = Cmd(_w, "SELECT change_pct, is_switch FROM captures WHERE id=$id", ("$id", id));
            using var rd = cmd.ExecuteReader();
            if (!rd.Read()) return (null, null);
            return (rd.IsDBNull(0) ? null : rd.GetDouble(0), rd.IsDBNull(1) ? null : rd.GetInt32(1) == 1);
        }
    }

    /// <summary>Tests: runs only the thinning step at a given clock.</summary>
    internal int ThinForTest(RetentionSettings r, DateTimeOffset now)
    {
        lock (_wl)
        {
            long cutoff = now.AddDays(-Math.Max(1, r.Days)).ToUnixTimeMilliseconds();
            return ThinLocked(r, now, cutoff);
        }
    }
}
