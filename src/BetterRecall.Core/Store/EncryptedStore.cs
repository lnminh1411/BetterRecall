// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Diagnostics;
using System.Globalization;
using System.Runtime.InteropServices;
using System.Security.Cryptography;
using System.Text;
using System.Text.Json;
using System.Text.RegularExpressions;
using BetterRecall.Contracts;
using Microsoft.Data.Sqlite;

namespace BetterRecall.Core.Store;

/// <summary>
/// Encrypted store: store.db plus keyframes\YYYY-MM.db, SQLite3MultipleCiphers with the SQLCipher scheme
/// (legacy=4) and a raw 256-bit key, WAL, secure_delete=ON. Rows stay invisible to Search, Get, Timeline and
/// GetKeyframe until state = ready. All public methods are thread-safe.
/// </summary>
public sealed partial class EncryptedStore : IStore
{
    private const int StatePending = 0, StateReady = 1, StateFailed = 2;
    private const int KindScreen = 0, KindAudio = 1, KindReplay = 2;
    /// <summary>Every kind bit (VectorFilter.KindMask, FTS filter): screen 1, audio 2, replay 4.</summary>
    private const int AllKinds = 7;

    private static CaptureKind KindOf(int k) => k switch { KindAudio => CaptureKind.Audio, KindReplay => CaptureKind.Replay, _ => CaptureKind.Screen };
    private static int KindBit(CaptureKind k) => k switch { CaptureKind.Audio => 2, CaptureKind.Replay => 4, _ => 1 };
    private const int RrfK = 60;
    private const int FtsCandidates = 200;
    private const int VectorScanTop = 400;
    private const int VectorKeep = 200;
    private const int SnippetMax = 300;

    private static int _sqliteInit;

    private readonly string _home;
    private readonly string _kfDir;
    private readonly Func<byte[]> _key;
    private readonly IVectorIndex _index;
    private readonly Lock _wl = new();
    private readonly Lock _rl = new();
    private readonly Lock _vl = new();
    private readonly Dictionary<string, SqliteConnection> _kf = new(StringComparer.Ordinal);
    // Embedding job leases: rid -> TickCount64 when handed out (A-M1: a lease expires, see EmbeddingLeaseTimeout).
    private readonly Dictionary<long, long> _inflight = [];
    private SqliteConnection _w = null!;
    private SqliteConnection _r = null!;
    private SqliteConnection _rv = null!; // vector rerank reader, so FTS and vector stages run in parallel
    private EmbeddingInfo? _space;
    private bool _disposed;
    // Purged time windows that refuse items still in flight (B12): "purge last N" (from = MinValue: anything stamped before
    // its end) and ranges ending about now. Each expires FenceLifetime after it was set, by the monotonic clock, so a wall
    // clock that steps back after the purge cannot refuse new captures for long.
    private readonly List<(long From, long To, long SetAt)> _fences = [];
    private readonly List<(string Id, long Ts)> _deleted = [];   // rows deleted under _wl, for the next Purged notice

    /// <summary>A range purge ending this close to now (or later) fences its window like "purge last N".</summary>
    internal static readonly TimeSpan FenceSlack = TimeSpan.FromMinutes(5);
    /// <summary>How long a fence refuses in-flight items: longer than OCR, encode and transcription can hold one.</summary>
    internal static readonly TimeSpan FenceLifetime = TimeSpan.FromMinutes(15);
    /// <summary>Monotonic milliseconds for fence expiry. Tests replace it.</summary>
    internal Func<long> MonoMs { get; set; } = () => Environment.TickCount64;

    /// <summary>Raised after rows were deleted (see PurgeNotice for the reasons), outside the store lock. CoreHost forwards
    /// it as ICoreHost.Purged.</summary>
    public event Action<PurgeNotice>? Purged;

    /// <summary>Opens (or creates) the store. key returns a fresh copy of the 32-byte SQLite key; it is zeroed after use.</summary>
    public EncryptedStore(string home, Func<byte[]> key, IVectorIndex? index = null)
    {
        if (Interlocked.Exchange(ref _sqliteInit, 1) == 0) SQLitePCL.Batteries_V2.Init();
        _home = home;
        _kfDir = DataPaths.KeyframesDir(home);
        _key = key;
        _index = index ?? new BinaryVectorIndex();
        Directory.CreateDirectory(home);
        Directory.CreateDirectory(_kfDir);
        OpenMain();
    }

    public IVectorIndex Index => _index;
    public EmbeddingInfo? Space { get { lock (_wl) return _space; } }

    // ---------------------------------------------------------------- connections

    private SqliteConnection OpenEncrypted(string path, bool incrementalVacuum = false)
    {
        var conn = new SqliteConnection(new SqliteConnectionStringBuilder { DataSource = path, Pooling = false, Mode = SqliteOpenMode.ReadWriteCreate }.ToString());
        conn.Open();
        var k = _key();
        try
        {
            Exec(conn, "PRAGMA cipher='sqlcipher'");
            Exec(conn, "PRAGMA legacy=4");
            Exec(conn, "PRAGMA key=\"x'" + Convert.ToHexString(k) + "'\"");
        }
        finally { CryptographicOperations.ZeroMemory(k); }
        try
        {
            Scalar(conn, "SELECT count(*) FROM sqlite_master");
            // Only takes effect on a new, empty file, and only before WAL mode writes the header.
            if (incrementalVacuum) Exec(conn, "PRAGMA auto_vacuum=INCREMENTAL");
            Exec(conn, "PRAGMA journal_mode=WAL");
            Exec(conn, "PRAGMA secure_delete=ON");
            Exec(conn, "PRAGMA synchronous=NORMAL");
            Exec(conn, "PRAGMA busy_timeout=5000");
        }
        catch { conn.Dispose(); throw; }
        return conn;
    }

    private void OpenMain()
    {
        var path = DataPaths.StoreDb(_home);
        _w = OpenEncrypted(path);
        Exec(_w, """
            CREATE TABLE IF NOT EXISTS meta(k TEXT PRIMARY KEY, v TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS captures(
              rid INTEGER PRIMARY KEY,
              id TEXT NOT NULL UNIQUE,
              ts INTEGER NOT NULL,
              kind INTEGER NOT NULL,
              app TEXT NOT NULL,
              app_lc TEXT NOT NULL,
              exe TEXT NOT NULL,
              title TEXT NOT NULL,
              text TEXT,
              source TEXT,
              language TEXT,
              monitor INTEGER NOT NULL,
              width INTEGER NOT NULL,
              height INTEGER NOT NULL,
              dhash INTEGER NOT NULL,
              duration_ms INTEGER NOT NULL,
              state INTEGER NOT NULL,
              has_keyframe INTEGER NOT NULL,
              keep_keyframe INTEGER NOT NULL,
              kf_month TEXT);
            CREATE INDEX IF NOT EXISTS ix_captures_state_ts ON captures(state, ts);
            CREATE INDEX IF NOT EXISTS ix_captures_ts ON captures(ts);
            CREATE INDEX IF NOT EXISTS ix_captures_app ON captures(app_lc, ts);
            CREATE INDEX IF NOT EXISTS ix_captures_month ON captures(kf_month);
            CREATE TABLE IF NOT EXISTS vectors(
              rid INTEGER PRIMARY KEY,
              model_id TEXT NOT NULL,
              revision TEXT NOT NULL,
              dim INTEGER NOT NULL,
              vec BLOB NOT NULL,
              bin BLOB NOT NULL);
            CREATE TABLE IF NOT EXISTS jobs(rid INTEGER PRIMARY KEY, created INTEGER NOT NULL);
            CREATE VIRTUAL TABLE IF NOT EXISTS fts USING fts5(title, app, text, tokenize='unicode61 remove_diacritics 2');
            CREATE TABLE IF NOT EXISTS audit(
              seq INTEGER PRIMARY KEY,
              ts INTEGER NOT NULL,
              pipe TEXT NOT NULL,
              pid INTEGER NOT NULL,
              image TEXT NOT NULL,
              verb TEXT NOT NULL,
              count INTEGER NOT NULL,
              sha TEXT NOT NULL);
            CREATE INDEX IF NOT EXISTS ix_audit_ts ON audit(ts);
            """);
        Exec(_w, "INSERT OR IGNORE INTO meta(k, v) VALUES('schema', '1')");
        Migrate();
        _r = OpenEncrypted(path);
        Exec(_r, "PRAGMA query_only=ON");
        Exec(_r, "PRAGMA cache_size=-8192");
        _rv = OpenEncrypted(path);
        Exec(_rv, "PRAGMA query_only=ON");
        Exec(_rv, "PRAGMA cache_size=-16384"); // keeps the vectors B-tree interior pages decrypted in RAM
        _space = ReadSpace();
        LoadIndex();
    }

    /// <summary>
    /// Schema 2 (0.2.0): captures.site (browser host) and the search_history table. ADD COLUMN with a constant
    /// default rewrites no rows, so it is instant on large stores.
    /// </summary>
    private void Migrate()
    {
        var cols = new HashSet<string>(StringComparer.Ordinal);
        using (var cmd = Cmd(_w, "SELECT name FROM pragma_table_info('captures')"))
        using (var rd = cmd.ExecuteReader())
            while (rd.Read()) cols.Add(rd.GetString(0));
        if (!cols.Contains("site")) Exec(_w, "ALTER TABLE captures ADD COLUMN site TEXT NOT NULL DEFAULT ''");
        MigrateStorage(cols);
        Exec(_w, """
            CREATE INDEX IF NOT EXISTS ix_captures_site ON captures(site, ts);
            CREATE TABLE IF NOT EXISTS search_history(q TEXT PRIMARY KEY, ts INTEGER NOT NULL);
            CREATE INDEX IF NOT EXISTS ix_history_ts ON search_history(ts);
            UPDATE meta SET v='2' WHERE k='schema' AND v='1';
            """);
        // 0.2 batch 4: frames sampled from saved replays (captures.kind 2) point at their replay and offset.
        Exec(_w, """
            CREATE TABLE IF NOT EXISTS replay_frames(rid INTEGER PRIMARY KEY, replay_id TEXT NOT NULL, offset_ms INTEGER NOT NULL);
            CREATE INDEX IF NOT EXISTS ix_replay_frames_replay ON replay_frames(replay_id);
            """);
        MigrateChats();
        MigrateFts();
    }

    /// <summary>
    /// FTS5 keeps the terms of deleted rows in its index segments unless the persistent secure-delete option is on.
    /// Turned on once per store; an existing store also gets one optimize, which merges the old segments so the terms
    /// of earlier deletes go too (PRAGMA secure_delete then zeroes the freed pages).
    /// </summary>
    private void MigrateFts()
    {
        if (Scalar(_w, "SELECT v FROM meta WHERE k='fts_secure_delete'") is not null) return;
        Exec(_w, "INSERT INTO fts(fts, rank) VALUES('secure-delete', 1)");
        Exec(_w, "INSERT INTO fts(fts) VALUES('optimize')");
        Exec(_w, "INSERT OR REPLACE INTO meta(k, v) VALUES('fts_secure_delete', '1')");
        Exec(_w, "PRAGMA wal_checkpoint(TRUNCATE)");
    }

    private SqliteConnection? Kf(string month, bool create)
    {
        if (_kf.TryGetValue(month, out var c)) return c;
        var path = KfPath(month);
        if (!create && !File.Exists(path)) return null;
        // New month files free deleted pages with incremental_vacuum (no temp copy, works on a nearly full disk).
        c = OpenEncrypted(path, incrementalVacuum: !File.Exists(path));
        Exec(c, "CREATE TABLE IF NOT EXISTS kf(rid INTEGER PRIMARY KEY, mime TEXT NOT NULL, width INTEGER NOT NULL, height INTEGER NOT NULL, data BLOB NOT NULL)");
        _kf[month] = c;
        return c;
    }

    private string KfPath(string month) => Path.Combine(_kfDir, month + ".db");

    private static string Month(long tsMs) => DateTimeOffset.FromUnixTimeMilliseconds(tsMs).UtcDateTime.ToString("yyyy-MM", CultureInfo.InvariantCulture);

    private List<string> KfMonths()
    {
        if (!Directory.Exists(_kfDir)) return [];
        return Directory.GetFiles(_kfDir, "????-??.db").Select(p => Path.GetFileNameWithoutExtension(p)).Order(StringComparer.Ordinal).ToList();
    }

    private void DeleteKfFile(string month)
    {
        if (_kf.Remove(month, out var c)) { try { Exec(c, "PRAGMA wal_checkpoint(TRUNCATE)"); } catch { } c.Dispose(); }
        SqliteConnection.ClearAllPools();
        var path = KfPath(month);
        foreach (var p in new[] { path, path + "-wal", path + "-shm" })
            try { if (File.Exists(p)) File.Delete(p); } catch (Exception ex) { Log.Warn("keyframe file delete failed", ex); }
    }

    // ---------------------------------------------------------------- helpers

    private static void Exec(SqliteConnection c, string sql, params (string, object?)[] ps)
    {
        using var cmd = c.CreateCommand();
        cmd.CommandText = sql;
        foreach (var (n, v) in ps) cmd.Parameters.AddWithValue(n, v ?? DBNull.Value);
        cmd.ExecuteNonQuery();
    }

    private static object? Scalar(SqliteConnection c, string sql, params (string, object?)[] ps)
    {
        using var cmd = c.CreateCommand();
        cmd.CommandText = sql;
        foreach (var (n, v) in ps) cmd.Parameters.AddWithValue(n, v ?? DBNull.Value);
        var r = cmd.ExecuteScalar();
        return r is DBNull ? null : r;
    }

    private static SqliteCommand Cmd(SqliteConnection c, string sql, params (string, object?)[] ps)
    {
        var cmd = c.CreateCommand();
        cmd.CommandText = sql;
        foreach (var (n, v) in ps) cmd.Parameters.AddWithValue(n, v ?? DBNull.Value);
        return cmd;
    }

    /// <summary>Begins a transaction on the write connection. Caller holds _wl. SqliteTransaction runs BEGIN and then
    /// registers a native rollback hook; when the hook throws (its callback thunk refused under ACG), no transaction
    /// object exists to roll back and the connection stays inside BEGIN, so every later write failed with "cannot start
    /// a transaction within a transaction". Rolls back any such leftover before and after, so one error cannot wedge
    /// the store.</summary>
    private SqliteTransaction BeginWrite()
    {
        RollbackLeftover(_w);
        try { return _w.BeginTransaction(); }
        catch { RollbackLeftover(_w); throw; }
    }

    private static void RollbackLeftover(SqliteConnection c)
    {
        try
        {
            if (c.Handle is { } h && SQLitePCL.raw.sqlite3_get_autocommit(h) == 0)
            {
                Exec(c, "ROLLBACK");
                Log.Warn("store: rolled back a transaction left open by an earlier error");
            }
        }
        catch (Exception ex) { Log.Warn("store: rollback of a leftover transaction failed", ex); }
    }

    /// <summary>Creates the native callback thunks SQLite transactions need (the rollback hook bridge) on a throwaway
    /// in-memory database. Call before Arbitrary Code Guard is applied: under ACG the runtime cannot allocate a new
    /// thunk, and the first transaction afterwards throws PlatformNotSupportedException.</summary>
    public static void WarmNativeCallbacks()
    {
        if (Interlocked.Exchange(ref _sqliteInit, 1) == 0) SQLitePCL.Batteries_V2.Init();
        using var c = new SqliteConnection("Data Source=:memory:;Pooling=False");
        c.Open();
        using (var tx = c.BeginTransaction())
        {
            Exec(c, "CREATE TEMP TABLE warm(x)");
            tx.Rollback();
        }
        using (c.BeginTransaction()) { }   // dispose path: rollback without an explicit call
    }

    private static string InList(IEnumerable<long> rids) => string.Join(",", rids.Select(r => r.ToString(CultureInfo.InvariantCulture)));

    private static float[] ToFloats(byte[] blob) => MemoryMarshal.Cast<byte, float>(blob).ToArray();

    private static byte[] ToBytes(ReadOnlySpan<float> v) => MemoryMarshal.AsBytes(v).ToArray();

    private static float[] Normalize(ReadOnlySpan<float> v)
    {
        var r = v.ToArray();
        double n = 0;
        foreach (var x in r) n += (double)x * x;
        if (n > 0) { float inv = (float)(1 / Math.Sqrt(n)); for (int i = 0; i < r.Length; i++) r[i] *= inv; }
        return r;
    }

    private void ThrowIfDisposed() => ObjectDisposedException.ThrowIf(_disposed, this);

    private long? Rid(string id)
        => Scalar(_w, "SELECT rid FROM captures WHERE id=$id", ("$id", id)) is long r ? r : null;

    // ---------------------------------------------------------------- embedding space and index

    private EmbeddingInfo? ReadSpace()
    {
        if (Scalar(_w, "SELECT v FROM meta WHERE k='space'") is not string s) return null;
        var p = s.Split('\n');
        return p.Length == 3 && int.TryParse(p[2], CultureInfo.InvariantCulture, out var d) ? new EmbeddingInfo(p[0], p[1], d) : null;
    }

    private void WriteSpace(EmbeddingInfo info)
    {
        Exec(_w, "INSERT INTO meta(k, v) VALUES('space', $v) ON CONFLICT(k) DO UPDATE SET v=excluded.v",
            ("$v", $"{info.ModelId}\n{info.Revision}\n{info.Dim.ToString(CultureInfo.InvariantCulture)}"));
        _space = info;
    }

    private void LoadIndex()
    {
        _index.Clear();
        if (_space == null) return;
        var sw = Stopwatch.StartNew();
        using var cmd = Cmd(_w, """
            SELECT v.rid, v.dim, v.bin, c.ts, c.kind, c.app_lc, c.site FROM vectors v JOIN captures c ON c.rid = v.rid
            WHERE c.state = 1 AND v.model_id = $m AND v.revision = $r
            """, ("$m", _space.ModelId), ("$r", _space.Revision));
        using var rd = cmd.ExecuteReader();
        var bi = _index as BinaryVectorIndex;
        int n = 0;
        while (rd.Read())
        {
            var bin = (byte[])rd.GetValue(2);
            var words = MemoryMarshal.Cast<byte, ulong>(bin);
            if (bi != null) bi.UpsertBits(rd.GetInt64(0), words, rd.GetInt32(1), rd.GetInt64(3), rd.GetInt32(4), rd.GetString(5), rd.GetString(6));
            n++;
        }
        Log.Info($"vector index loaded: {n} rows in {sw.ElapsedMilliseconds} ms");
    }

    private bool InSpace(string model, string revision) => _space != null && _space.ModelId == model && _space.Revision == revision;

    public void EnsureEmbeddingSpace(EmbeddingInfo info)
    {
        ArgumentNullException.ThrowIfNull(info);
        if (info.Dim <= 0 || info.Dim > 8192) throw new ArgumentException("Bad dim.");
        lock (_wl)
        {
            ThrowIfDisposed();
            var old = _space;
            if (old != null && old.ModelId == info.ModelId && old.Revision == info.Revision)
            {
                if (info.Dim < old.Dim) TruncateVectors(info.Dim);
                if (info.Dim > old.Dim) QueueShortVectors(info);   // before WriteSpace: a failure here is retried next call
                if (info.Dim != old.Dim) { WriteSpace(info); Log.Info($"embedding dim {old.Dim} -> {info.Dim}"); }
                if (info.Dim < old.Dim) { lock (_rl) LoadIndex(); }
                return;
            }
            WriteSpace(info);
            if (old == null && (long)(Scalar(_w, "SELECT count(*) FROM vectors WHERE model_id<>$m OR revision<>$r", ("$m", info.ModelId), ("$r", info.Revision)) ?? 0L) == 0)
            {
                LoadIndex();
                return;
            }
            // New model: rows that can still be embedded (keyframe or transcript present) get a job again.
            long now = DateTimeOffset.UtcNow.ToUnixTimeMilliseconds();
            using (var tx = BeginWrite())
            {
                Exec(_w, """
                    INSERT OR IGNORE INTO jobs(rid, created)
                    SELECT c.rid, $now FROM captures c LEFT JOIN vectors v ON v.rid = c.rid
                    WHERE c.state = 1
                      AND (v.rid IS NULL OR v.model_id <> $m OR v.revision <> $r)
                      AND ((c.kind IN (0, 2) AND c.has_keyframe = 1) OR (c.kind = 1 AND c.text IS NOT NULL AND c.text <> ''))
                    """, ("$now", now), ("$m", info.ModelId), ("$r", info.Revision));
                tx.Commit();
            }
            Log.Info("embedding model changed, rows queued for re-embedding");
            lock (_rl) LoadIndex();
        }
    }

    /// <summary>Going back up in dim (C5): vectors truncated by an earlier lower dim cannot grow back, so rows that can
    /// still be embedded (keyframe or transcript kept) get a job. They stay searchable at their old dim meanwhile.</summary>
    private void QueueShortVectors(EmbeddingInfo info)
    {
        long now = DateTimeOffset.UtcNow.ToUnixTimeMilliseconds();
        int n;
        using (var tx = BeginWrite())
        {
            Exec(_w, """
                INSERT OR IGNORE INTO jobs(rid, created)
                SELECT c.rid, $now FROM captures c JOIN vectors v ON v.rid = c.rid
                WHERE c.state = 1 AND v.model_id = $m AND v.revision = $r AND v.dim < $d
                  AND ((c.kind IN (0, 2) AND c.has_keyframe = 1) OR (c.kind = 1 AND c.text IS NOT NULL AND c.text <> ''))
                """, ("$now", now), ("$m", info.ModelId), ("$r", info.Revision), ("$d", info.Dim));
            n = (int)(long)(Scalar(_w, "SELECT changes()") ?? 0L);
            tx.Commit();
        }
        Log.Info($"{n} rows with shorter vectors queued for re-embedding at {info.Dim}-d");
    }

    private void TruncateVectors(int dim)
    {
        var rows = new List<(long Rid, byte[] Vec)>();
        using (var cmd = Cmd(_w, "SELECT rid, vec FROM vectors WHERE dim > $d", ("$d", dim)))
        using (var rd = cmd.ExecuteReader())
            while (rd.Read()) rows.Add((rd.GetInt64(0), (byte[])rd.GetValue(1)));
        using var tx = BeginWrite();
        foreach (var (rid, vec) in rows)
        {
            var v = Normalize(ToFloats(vec).AsSpan(0, dim));
            Exec(_w, "UPDATE vectors SET dim=$d, vec=$v, bin=$b WHERE rid=$rid",
                ("$d", dim), ("$v", ToBytes(v)), ("$b", BinaryVectorIndex.SignBitsBytes(v)), ("$rid", rid));
        }
        tx.Commit();
        Log.Info($"truncated {rows.Count} vectors to {dim}-d");
    }

    // ---------------------------------------------------------------- ingest

    public string AddScreenCapture(NewScreenCapture c)
    {
        ArgumentNullException.ThrowIfNull(c);
        ArgumentNullException.ThrowIfNull(c.Image);
        lock (_wl)
        {
            ThrowIfDisposed();
            ThrowIfFenced(c.Ts);
            var id = Ids.New('s', c.Ts);
            long ts = c.Ts.ToUnixTimeMilliseconds();
            var month = Month(ts);
            long rid;
            using (var tx = BeginWrite())
            {
                rid = (long)Scalar(_w, """
                    INSERT INTO captures(id, ts, kind, app, app_lc, exe, title, text, monitor, width, height, dhash, duration_ms, state, has_keyframe, keep_keyframe, kf_month, site, change_pct, is_switch)
                    VALUES($id, $ts, 0, $app, $applc, $exe, $title, $text, $mon, $w, $h, $dh, 0, 0, 1, $keep, $month, $site, $chg, $sw) RETURNING rid
                    """, ("$site", NormalizeSite(c.Site)), ("$text", string.IsNullOrWhiteSpace(c.Text) ? null : c.Text), ("$chg", c.Change is double chg ? Math.Clamp(chg, 0, 100) : null), ("$sw", c.Change is null && !c.Switch ? null : c.Switch ? 1 : 0), ("$id", id), ("$ts", ts), ("$app", c.App ?? ""), ("$applc", (c.App ?? "").ToLowerInvariant()), ("$exe", c.ExePath ?? ""),
                    ("$title", c.Title ?? ""), ("$mon", c.Monitor), ("$w", c.Width), ("$h", c.Height), ("$dh", unchecked((long)c.DHash)),
                    ("$keep", c.KeepKeyframe ? 1 : 0), ("$month", month))!;
                Exec(_w, "INSERT INTO jobs(rid, created) VALUES($rid, $now)", ("$rid", rid), ("$now", DateTimeOffset.UtcNow.ToUnixTimeMilliseconds()));
                tx.Commit();
            }
            try
            {
                Exec(Kf(month, create: true)!, "INSERT OR REPLACE INTO kf(rid, mime, width, height, data) VALUES($rid, $mime, $w, $h, $data)",
                    ("$rid", rid), ("$mime", c.ImageMime ?? "image/webp"), ("$w", c.Width), ("$h", c.Height), ("$data", c.Image));
            }
            catch
            {
                Exec(_w, "DELETE FROM jobs WHERE rid=$rid", ("$rid", rid));
                Exec(_w, "DELETE FROM captures WHERE rid=$rid", ("$rid", rid));
                throw;
            }
            return id;
        }
    }

    /// <summary>Frame of a saved replay: a kind-2 row with its keyframe (always kept) plus a replay_frames link.</summary>
    public string AddReplayFrame(NewReplayFrame f)
    {
        ArgumentNullException.ThrowIfNull(f);
        ArgumentNullException.ThrowIfNull(f.Image);
        if (string.IsNullOrWhiteSpace(f.ReplayId) || f.ReplayId.Length > 64) throw new ArgumentException("Bad replay id.");
        lock (_wl)
        {
            ThrowIfDisposed();
            ThrowIfFenced(f.Ts);
            // A carry-in sample is re-stamped to the clip start: the pixels are older, and the fence checks those too (N4).
            if (f.CapturedAt is { } grabbed && grabbed < f.Ts) ThrowIfFenced(grabbed);
            var id = Ids.New('r', f.Ts);
            long ts = f.Ts.ToUnixTimeMilliseconds();
            var month = Month(ts);
            long rid;
            using (var tx = BeginWrite())
            {
                rid = (long)Scalar(_w, """
                    INSERT INTO captures(id, ts, kind, app, app_lc, exe, title, monitor, width, height, dhash, duration_ms, state, has_keyframe, keep_keyframe, kf_month, site)
                    VALUES($id, $ts, 2, $app, $applc, '', $title, $mon, $w, $h, 0, 0, 0, 1, 1, $month, $site) RETURNING rid
                    """, ("$site", NormalizeSite(f.Site)), ("$id", id), ("$ts", ts), ("$app", f.App ?? ""), ("$applc", (f.App ?? "").ToLowerInvariant()),
                    ("$title", f.Title ?? ""), ("$mon", f.Monitor), ("$w", f.Width), ("$h", f.Height), ("$month", month))!;
                Exec(_w, "INSERT INTO replay_frames(rid, replay_id, offset_ms) VALUES($rid, $rep, $off)", ("$rid", rid), ("$rep", f.ReplayId), ("$off", Math.Max(0, f.OffsetMs)));
                Exec(_w, "INSERT INTO jobs(rid, created) VALUES($rid, $now)", ("$rid", rid), ("$now", DateTimeOffset.UtcNow.ToUnixTimeMilliseconds()));
                tx.Commit();
            }
            try
            {
                Exec(Kf(month, create: true)!, "INSERT OR REPLACE INTO kf(rid, mime, width, height, data) VALUES($rid, $mime, $w, $h, $data)",
                    ("$rid", rid), ("$mime", f.ImageMime ?? "image/webp"), ("$w", f.Width), ("$h", f.Height), ("$data", f.Image));
            }
            catch
            {
                int before = _deleted.Count;
                DeleteRowsLocked([rid]);
                // Never visible: nothing to tell. Remove only this row; earlier entries (a maintenance slice's deletes) stay.
                _deleted.RemoveRange(before, _deleted.Count - before);
                throw;
            }
            return id;
        }
    }

    public int DeleteReplayFrames(string replayId)
    {
        if (string.IsNullOrWhiteSpace(replayId)) return 0;
        PurgeNotice? notice;
        int n;
        lock (_wl)
        {
            ThrowIfDisposed();
            var rids = new List<long>();
            using (var cmd = Cmd(_w, "SELECT rid FROM replay_frames WHERE replay_id=$r", ("$r", replayId)))
            using (var rd = cmd.ExecuteReader())
                while (rd.Read()) rids.Add(rd.GetInt64(0));
            n = DeleteRowsLocked(rids);
            if (n > 0) Log.Info($"replay deleted: {n} replay frames removed");
            notice = TakeNoticeLocked("replay");
        }
        Raise(notice);
        return n;
    }

    public string AddAudioCapture(NewAudioCapture c)
    {
        ArgumentNullException.ThrowIfNull(c);
        lock (_wl)
        {
            ThrowIfDisposed();
            ThrowIfFenced(c.Ts);
            var id = Ids.New('a', c.Ts);
            long ts = c.Ts.ToUnixTimeMilliseconds();
            using var tx = BeginWrite();
            long rid = (long)Scalar(_w, """
                INSERT INTO captures(id, ts, kind, app, app_lc, exe, title, text, source, language, monitor, width, height, dhash, duration_ms, state, has_keyframe, keep_keyframe, kf_month)
                VALUES($id, $ts, 1, $app, $applc, '', '', $text, $src, $lang, -1, 0, 0, 0, $dur, 1, 0, 0, NULL) RETURNING rid
                """, ("$id", id), ("$ts", ts), ("$app", c.App ?? ""), ("$applc", (c.App ?? "").ToLowerInvariant()), ("$text", c.Text ?? ""),
                ("$src", c.Source ?? ""), ("$lang", c.Language ?? ""), ("$dur", c.DurationMs))!;
            // Audio passes no image gate, so it is searchable by text at once; the vector follows.
            Exec(_w, "INSERT INTO fts(rowid, title, app, text) VALUES($rid, '', $app, $text)", ("$rid", rid), ("$app", c.App ?? ""), ("$text", c.Text ?? ""));
            if (!string.IsNullOrWhiteSpace(c.Text))
                Exec(_w, "INSERT INTO jobs(rid, created) VALUES($rid, $now)", ("$rid", rid), ("$now", DateTimeOffset.UtcNow.ToUnixTimeMilliseconds()));
            tx.Commit();
            return id;
        }
    }

    /// <summary>How long a job handed out by <see cref="TakePendingEmbeddings"/> is held back from the next take
    /// (A-M1). The agent's scheduler takes jobs only once it holds none, so a lease older than this belongs to jobs it
    /// dropped without a reply (panic clears its RAM queue): they are handed out again instead of staying hidden, and
    /// out of status.queue.pending's drain, until the agent restarts. Contract this relies on: one consumer, which takes
    /// only while it holds no job (a second take of a held job costs a duplicate embed, never a lost row).</summary>
    public TimeSpan EmbeddingLeaseTimeout { get; set; } = TimeSpan.FromMinutes(2);

    private bool Leased(long rid, long now) => _inflight.TryGetValue(rid, out var at) && now - at < (long)EmbeddingLeaseTimeout.TotalMilliseconds;

    public IReadOnlyList<PendingJob> TakePendingEmbeddings(int max)
    {
        if (max <= 0) return [];
        lock (_wl)
        {
            ThrowIfDisposed();
            long leaseNow = Environment.TickCount64;
            if (_inflight.Count > 0)
                foreach (var rid in _inflight.Where(kv => !Leased(kv.Key, leaseNow)).Select(kv => kv.Key).ToList()) _inflight.Remove(rid);
            var rows = new List<(long Rid, string Id, int Kind, string? Text, string? Month, bool HasKf)>();
            using (var cmd = Cmd(_w, """
                SELECT j.rid, c.id, c.kind, c.text, c.kf_month, c.has_keyframe FROM jobs j JOIN captures c ON c.rid = j.rid
                ORDER BY c.ts LIMIT $n
                """, ("$n", max + _inflight.Count)))
            using (var rd = cmd.ExecuteReader())
                while (rd.Read())
                    rows.Add((rd.GetInt64(0), rd.GetString(1), rd.GetInt32(2), rd.IsDBNull(3) ? null : rd.GetString(3), rd.IsDBNull(4) ? null : rd.GetString(4), rd.GetInt32(5) == 1));
            var jobs = new List<PendingJob>();
            foreach (var r in rows)
            {
                if (jobs.Count >= max) break;
                if (Leased(r.Rid, leaseNow)) continue;
                if (r.Kind == KindAudio)
                {
                    jobs.Add(new PendingJob(r.Id, CaptureKind.Audio, null, null, r.Text ?? ""));
                    _inflight[r.Rid] = leaseNow;
                    continue;
                }
                (byte[] Data, string Mime)? img = null;
                if (r.HasKf && r.Month != null && Kf(r.Month, create: false) is { } kc)
                {
                    using var cmd = Cmd(kc, "SELECT data, mime FROM kf WHERE rid=$rid", ("$rid", r.Rid));
                    using var rd = cmd.ExecuteReader();
                    if (rd.Read()) img = ((byte[])rd.GetValue(0), rd.GetString(1));
                }
                if (img == null)
                {
                    // Keyframe gone (retention, size cap): nothing left to embed.
                    FailLocked(r.Rid, "keyframe missing");
                    continue;
                }
                jobs.Add(new PendingJob(r.Id, KindOf(r.Kind), img.Value.Data, img.Value.Mime, null));
                _inflight[r.Rid] = leaseNow;
            }
            return jobs;
        }
    }

    public void SetEmbedding(string id, ReadOnlySpan<float> vector, EmbeddingInfo info)
    {
        ArgumentNullException.ThrowIfNull(info);
        if (vector.Length != info.Dim || vector.Length == 0) throw new ArgumentException("Vector length must equal info.Dim.");
        var v = Normalize(vector);
        lock (_wl)
        {
            ThrowIfDisposed();
            using var cmd0 = Cmd(_w, "SELECT rid, state, kind, title, app, text, keep_keyframe, kf_month, ts, app_lc, site FROM captures WHERE id=$id", ("$id", id));
            using var rd = cmd0.ExecuteReader();
            if (!rd.Read()) { Log.Debug("SetEmbedding for a row that no longer exists"); return; }
            long rid = rd.GetInt64(0);
            int state = rd.GetInt32(1), kind = rd.GetInt32(2);
            string title = rd.GetString(3), app = rd.GetString(4);
            string? text = rd.IsDBNull(5) ? null : rd.GetString(5);
            bool keep = rd.GetInt32(6) == 1;
            string? month = rd.IsDBNull(7) ? null : rd.GetString(7);
            long ts = rd.GetInt64(8);
            string appLc = rd.GetString(9);
            string site = rd.GetString(10);
            rd.Close();
            _inflight.Remove(rid);
            if (state == StateFailed) { Exec(_w, "DELETE FROM jobs WHERE rid=$rid", ("$rid", rid)); return; }
            if (_space == null) WriteSpace(info);
            using (var tx = BeginWrite())
            {
                Exec(_w, """
                    INSERT INTO vectors(rid, model_id, revision, dim, vec, bin) VALUES($rid, $m, $r, $d, $v, $b)
                    ON CONFLICT(rid) DO UPDATE SET model_id=excluded.model_id, revision=excluded.revision, dim=excluded.dim, vec=excluded.vec, bin=excluded.bin
                    """, ("$rid", rid), ("$m", info.ModelId), ("$r", info.Revision), ("$d", info.Dim), ("$v", ToBytes(v)), ("$b", BinaryVectorIndex.SignBitsBytes(v)));
                Exec(_w, "DELETE FROM jobs WHERE rid=$rid", ("$rid", rid));
                if (state == StatePending)
                {
                    Exec(_w, "UPDATE captures SET state=1 WHERE rid=$rid", ("$rid", rid));
                    Exec(_w, "INSERT INTO fts(rowid, title, app, text) VALUES($rid, $t, $a, $x)", ("$rid", rid), ("$t", title), ("$a", app), ("$x", text));
                }
                if (kind == KindScreen && !keep) Exec(_w, "UPDATE captures SET has_keyframe=0 WHERE rid=$rid", ("$rid", rid));
                tx.Commit();
            }
            if (kind == KindScreen && !keep && month != null && Kf(month, false) is { } kc)
                Exec(kc, "DELETE FROM kf WHERE rid=$rid", ("$rid", rid));
            if (InSpace(info.ModelId, info.Revision)) _index.Upsert(rid, v, ts, kind, appLc, site);
            else _index.Remove(rid);
        }
    }

    public void FailEmbedding(string id, string reason)
    {
        lock (_wl)
        {
            ThrowIfDisposed();
            if (Rid(id) is not long rid) return;
            FailLocked(rid, reason);
        }
    }

    private void FailLocked(long rid, string reason)
    {
        _inflight.Remove(rid);
        using var cmd = Cmd(_w, "SELECT state, kind, keep_keyframe, kf_month FROM captures WHERE rid=$rid", ("$rid", rid));
        using var rd = cmd.ExecuteReader();
        if (!rd.Read()) return;
        int state = rd.GetInt32(0), kind = rd.GetInt32(1);
        bool keep = rd.GetInt32(2) == 1;
        string? month = rd.IsDBNull(3) ? null : rd.GetString(3);
        rd.Close();
        Exec(_w, "DELETE FROM jobs WHERE rid=$rid", ("$rid", rid));
        if (state == StatePending)
        {
            Exec(_w, "UPDATE captures SET state=2 WHERE rid=$rid", ("$rid", rid));
            if (kind == KindScreen && !keep && month != null && Kf(month, false) is { } kc)
            {
                Exec(kc, "DELETE FROM kf WHERE rid=$rid", ("$rid", rid));
                Exec(_w, "UPDATE captures SET has_keyframe=0 WHERE rid=$rid", ("$rid", rid));
            }
        }
        Log.Info($"embedding failed for a row ({SafeReason(reason)})");
    }

    private static string SafeReason(string? reason)
        => string.IsNullOrEmpty(reason) ? "no reason" : new string(reason.Where(ch => char.IsAsciiLetterOrDigit(ch) || ch is ' ' or '-' or '_').Take(40).ToArray());

    public void DropCapture(string id, string reason)
    {
        PurgeNotice? notice;
        lock (_wl)
        {
            ThrowIfDisposed();
            if (Rid(id) is not long rid) return;
            DeleteRowsLocked([rid]);
            Log.Info($"capture dropped ({SafeReason(reason)})");
            notice = TakeNoticeLocked("drop");
        }
        Raise(notice);
    }

    // ---------------------------------------------------------------- deletes

    /// <summary>Deletes rows, their vectors, FTS entries, jobs and keyframes. Caller holds _wl.</summary>
    private int DeleteRowsLocked(IReadOnlyList<long> rids)
    {
        int total = 0;
        for (int off = 0; off < rids.Count; off += 500)
        {
            var chunk = rids.Skip(off).Take(500).ToList();
            var list = InList(chunk);
            // Every row delete (purge of any scope, retention, thinning, space guard) drops the saved chats that quote it.
            DeleteChatsCitingLocked(chunk);
            using (var cmd = Cmd(_w, $"SELECT id, ts FROM captures WHERE rid IN ({list})"))
            using (var rd = cmd.ExecuteReader())
                while (rd.Read()) _deleted.Add((rd.GetString(0), rd.GetInt64(1)));
            var months = new Dictionary<string, List<long>>();
            using (var cmd = Cmd(_w, $"SELECT rid, kf_month FROM captures WHERE rid IN ({list}) AND +kf_month IS NOT NULL"))
            using (var rd = cmd.ExecuteReader())
                while (rd.Read())
                {
                    var m = rd.GetString(1);
                    if (!months.TryGetValue(m, out var l)) months[m] = l = [];
                    l.Add(rd.GetInt64(0));
                }
            using (var tx = BeginWrite())
            {
                Exec(_w, $"DELETE FROM fts WHERE rowid IN ({list})");
                Exec(_w, $"DELETE FROM vectors WHERE rid IN ({list})");
                Exec(_w, $"DELETE FROM jobs WHERE rid IN ({list})");
                Exec(_w, $"DELETE FROM replay_frames WHERE rid IN ({list})");
                using var del = Cmd(_w, $"DELETE FROM captures WHERE rid IN ({list})");
                total += del.ExecuteNonQuery();
                tx.Commit();
            }
            foreach (var (m, l) in months)
                if (Kf(m, false) is { } kc) Exec(kc, $"DELETE FROM kf WHERE rid IN ({InList(l)})");
            foreach (var rid in chunk) { _index.Remove(rid); _inflight.Remove(rid); }
        }
        return total;
    }

    private List<long> SelectScope(PurgeScope scope)
    {
        string where;
        var ps = new List<(string, object?)>();
        switch (scope.Kind?.ToLowerInvariant())
        {
            case "last":
            case "range":
                if (scope.From == null || scope.To == null) throw new ArgumentException("Range needs from and to.");
                where = "ts >= $from AND ts <= $to";
                ps.Add(("$from", scope.From.Value.ToUnixTimeMilliseconds()));
                ps.Add(("$to", scope.To.Value.ToUnixTimeMilliseconds()));
                break;
            case "app":
                if (string.IsNullOrWhiteSpace(scope.App)) throw new ArgumentException("App needed.");
                // With or without ".exe": captures store the process name, users type "claude.exe" (D13).
                where = "(app_lc = $app OR app_lc = $app2)";
                var appLc = scope.App.Trim().ToLowerInvariant();
                ps.Add(("$app", appLc));
                ps.Add(("$app2", appLc.EndsWith(".exe", StringComparison.Ordinal) ? appLc[..^4] : appLc + ".exe"));
                break;
            case "site":
                if (string.IsNullOrWhiteSpace(scope.Site)) throw new ArgumentException("Site needed.");
                where = "site = $site";
                ps.Add(("$site", NormalizeSite(scope.Site)));
                break;
            case "id":
                if (string.IsNullOrWhiteSpace(scope.Id)) throw new ArgumentException("Id needed.");
                where = "id = $id";
                ps.Add(("$id", scope.Id.Trim()));
                break;
            case "all":
                where = "1=1";
                break;
            default:
                throw new ArgumentException("Unknown purge scope.");
        }
        var rids = new List<long>();
        using var cmd = Cmd(_w, $"SELECT rid FROM captures WHERE {where}", [.. ps]);
        using var rd = cmd.ExecuteReader();
        while (rd.Read()) rids.Add(rd.GetInt64(0));
        return rids;
    }

    public int CountPurge(PurgeScope scope)
    {
        lock (_wl) { ThrowIfDisposed(); return SelectScope(scope).Count; }
    }

    /// <summary>Deletes the scope, then wal_checkpoint(TRUNCATE) on every open file. Purge(Last(Zero)) just checkpoints.</summary>
    public int Purge(PurgeScope scope)
    {
        PurgeNotice? notice;
        int n;
        using (EnterWritePriority())
        {
            ThrowIfDisposed();
            var rids = SelectScope(scope);
            // DeleteRowsLocked drops chats citing a deleted capture for every scope; time-window purges also delete
            // chats active in the window below.
            n = DeleteRowsLocked(rids);
            // Panic flush / "delete the last N" / a range up to about now: items captured before now but still being
            // encoded, OCR'd or transcribed would otherwise be stored after this purge, inside the window it just emptied.
            FenceLocked(scope);
            // A time-window purge (panic flush, "delete the last hour") also forgets searches typed in that window.
            if (scope.Kind is "last" or "range" && scope.From != null && scope.To != null)
            {
                Exec(_w, "DELETE FROM search_history WHERE ts >= $from AND ts <= $to",
                    ("$from", scope.From.Value.ToUnixTimeMilliseconds()), ("$to", scope.To.Value.ToUnixTimeMilliseconds()));
                // The audit rows stay (who searched, when), but not the hash of what was searched in the window (B9).
                Exec(_w, "UPDATE audit SET sha = '' WHERE verb = 'search' AND ts >= $from AND ts <= $to",
                    ("$from", scope.From.Value.ToUnixTimeMilliseconds()), ("$to", scope.To.Value.ToUnixTimeMilliseconds()));
                DeleteChatsInWindowLocked(scope.From.Value, scope.To.Value);   // chats active in the window go too
            }
            // App and site purges forget saved searches that name the app or site ("pornhub" after purge site pornhub.com).
            if (scope.Kind is "app" or "site") DeleteHistoryContainingLocked(HistoryNeedles(scope));
            CheckpointLocked();
            Log.Info($"purge ({scope.Kind}) removed {n} rows");
            bool window = scope.Kind is "last" or "range";
            notice = TakeNoticeLocked("purge", window ? scope.From : null, window ? scope.To : null);
        }
        Raise(notice);
        return n;
    }

    /// <summary>Blanks the hash of audit search rows whose hash is one of these (an app or site purge, B9). The rows stay.</summary>
    public int ClearAuditQueryHashes(IReadOnlyCollection<string> hashes)
    {
        if (hashes.Count == 0) return 0;
        int n = 0;
        lock (_wl)
        {
            ThrowIfDisposed();
            foreach (var h in hashes)
            {
                using var cmd = Cmd(_w, "UPDATE audit SET sha = '' WHERE verb = 'search' AND sha = $sha", ("$sha", h));
                n += cmd.ExecuteNonQuery();
            }
        }
        return n;
    }

    /// <summary>Fences a time-window purge: "last" refuses anything stamped before its end, a range ending within
    /// FenceSlack of now (or later) refuses its window up to now. Caller holds _wl.</summary>
    private void FenceLocked(PurgeScope scope)
    {
        if (scope.To is not { } to) return;
        var now = DateTimeOffset.UtcNow;
        long mono = MonoMs();
        long life = (long)FenceLifetime.TotalMilliseconds;
        _fences.RemoveAll(x => mono - x.SetAt >= life);
        if (scope.Kind == "last")
            _fences.Add((long.MinValue, to.ToUnixTimeMilliseconds(), mono));
        else if (scope.Kind == "range" && scope.From is { } from && to >= now - FenceSlack)
        {
            var end = to < now ? to : now;
            if (end > from) _fences.Add((from.ToUnixTimeMilliseconds(), end.ToUnixTimeMilliseconds(), mono));
        }
    }

    /// <summary>Caller holds _wl.</summary>
    private void ThrowIfFenced(DateTimeOffset ts)
    {
        if (_fences.Count == 0) return;
        long mono = MonoMs();
        long life = (long)FenceLifetime.TotalMilliseconds;
        _fences.RemoveAll(x => mono - x.SetAt >= life);
        long t = ts.ToUnixTimeMilliseconds();
        foreach (var (from, to, _) in _fences)
            if (t >= from && t <= to) throw new PurgedWindowException(ts);
    }

    /// <summary>The rows deleted since the last notice, as a notice; null when nothing was deleted and no window was
    /// purged. Caller holds _wl.</summary>
    private PurgeNotice? TakeNoticeLocked(string reason, DateTimeOffset? from = null, DateTimeOffset? to = null)
    {
        bool window = from is { } f && to is { } t && t > f;
        if (_deleted.Count == 0 && !window) return null;
        var ids = _deleted.Select(d => d.Id).ToArray();
        DateTimeOffset? newest = _deleted.Count == 0 ? null : DateTimeOffset.FromUnixTimeMilliseconds(_deleted.Max(d => d.Ts));
        _deleted.Clear();
        return new PurgeNotice(reason, ids, window ? from : null, window ? to : null, newest);
    }

    /// <summary>Raises Purged outside the store lock; a failing listener does not stop the others.</summary>
    private void Raise(PurgeNotice? notice)
    {
        if (notice is null || Purged is not { } h) return;
        foreach (var d in h.GetInvocationList())
        {
            try { ((Action<PurgeNotice>)d)(notice); }
            catch (Exception ex) { Log.Warn("purge listener failed", ex); }
        }
    }

    /// <summary>wal_checkpoint(TRUNCATE) on store.db and every open keyframe file (panic flush).</summary>
    public void Checkpoint()
    {
        lock (_wl) { ThrowIfDisposed(); CheckpointLocked(); }
    }

    private void CheckpointLocked()
    {
        try { Exec(_w, "PRAGMA wal_checkpoint(TRUNCATE)"); } catch (Exception ex) { Log.Warn("checkpoint failed", ex); }
        foreach (var c in _kf.Values)
            try { Exec(c, "PRAGMA wal_checkpoint(TRUNCATE)"); } catch (Exception ex) { Log.Warn("keyframe checkpoint failed", ex); }
    }

    // ---------------------------------------------------------------- reads

    private CaptureMeta ReadMeta(SqliteDataReader rd)
        => new(rd.GetString(0), DateTimeOffset.FromUnixTimeMilliseconds(rd.GetInt64(1)), KindOf(rd.GetInt32(2)),
            rd.GetString(3), rd.GetString(4), rd.GetInt32(5), rd.GetInt32(6), rd.GetInt32(7) == 1, !rd.IsDBNull(8), rd.GetString(9));

    public IReadOnlyList<CaptureMeta> Timeline(DateTimeOffset from, DateTimeOffset to, int limit)
    {
        limit = Math.Clamp(limit, 1, 100_000);
        lock (_rl)
        {
            ThrowIfDisposed();
            using var cmd = Cmd(_r, """
                SELECT c.id, c.ts, c.kind, c.app, c.title, c.monitor, c.duration_ms, c.has_keyframe, v.rid, c.site
                FROM captures c LEFT JOIN vectors v ON v.rid = c.rid
                WHERE c.state = 1 AND c.ts >= $from AND c.ts < $to AND c.kind <> 2 ORDER BY c.ts DESC LIMIT $n
                """, ("$from", from.ToUnixTimeMilliseconds()), ("$to", to.ToUnixTimeMilliseconds()), ("$n", limit));
            using var rd = cmd.ExecuteReader();
            var list = new List<CaptureMeta>();
            while (rd.Read()) list.Add(ReadMeta(rd));
            return list;
        }
    }

    public CaptureDetail? Get(string id)
    {
        lock (_rl)
        {
            ThrowIfDisposed();
            using var cmd = Cmd(_r, """
                SELECT c.id, c.ts, c.kind, c.app, c.title, c.text, c.has_keyframe, c.duration_ms, c.site, r.replay_id, r.offset_ms
                FROM captures c LEFT JOIN replay_frames r ON r.rid = c.rid WHERE c.id=$id AND c.state=1
                """, ("$id", id));
            using var rd = cmd.ExecuteReader();
            if (!rd.Read()) return null;
            bool audio = rd.GetInt32(2) == KindAudio;
            return new CaptureDetail(rd.GetString(0), DateTimeOffset.FromUnixTimeMilliseconds(rd.GetInt64(1)), KindOf(rd.GetInt32(2)),
                rd.GetString(3), rd.GetString(4), audio ? (rd.IsDBNull(5) ? "" : rd.GetString(5)) : !rd.IsDBNull(5) && rd.GetString(5).Length > 0 ? rd.GetString(5) : rd.GetString(4),
                rd.GetInt32(6) == 1, audio ? (rd.GetInt32(7) + 500) / 1000 : null, rd.GetString(8),
                rd.IsDBNull(9) ? null : rd.GetString(9), rd.IsDBNull(10) ? null : rd.GetInt32(10));
        }
    }

    public ThumbData? GetKeyframe(string id)
    {
        long rid;
        string month;
        lock (_rl)
        {
            ThrowIfDisposed();
            using var cmd = Cmd(_r, "SELECT rid, kf_month FROM captures WHERE id=$id AND state=1 AND has_keyframe=1 AND kf_month IS NOT NULL", ("$id", id));
            using var rd = cmd.ExecuteReader();
            if (!rd.Read()) return null;
            rid = rd.GetInt64(0);
            month = rd.GetString(1);
        }
        using (EnterWritePriority())
        {
            ThrowIfDisposed();
            if (Kf(month, false) is not { } kc) return null;
            using var cmd = Cmd(kc, "SELECT mime, width, height, data FROM kf WHERE rid=$rid", ("$rid", rid));
            using var rd = cmd.ExecuteReader();
            if (!rd.Read()) return null;
            return new ThumbData(id, rd.GetString(0), rd.GetInt32(1), rd.GetInt32(2), (byte[])rd.GetValue(3));
        }
    }

    // ---------------------------------------------------------------- search

    [GeneratedRegex(@"[\p{L}\p{N}\p{M}_]+")]
    private static partial Regex TokenRegex();

    internal static List<string> Tokens(string q) => TokenRegex().Matches(q).Select(m => m.Value).Where(t => t.Length > 0).Take(32).ToList();

    internal static string? FtsQuery(string q)
    {
        var tokens = Tokens(q);
        if (tokens.Count == 0) return null;
        var sb = new StringBuilder();
        for (int i = 0; i < tokens.Count; i++)
        {
            if (i > 0) sb.Append(" OR ");
            sb.Append('"').Append(tokens[i].Replace("\"", "\"\"")).Append('"');
            if (i == tokens.Count - 1 && tokens[i].Length >= 2) sb.Append('*');
        }
        return sb.ToString();
    }

    public SearchResponse Search(SearchRequest request, ReadOnlySpan<float> queryVector, int maxResults, int maxResponseBytes)
    {
        ArgumentNullException.ThrowIfNull(request);
        var sw = Stopwatch.StartNew();
        maxResults = Math.Max(1, maxResults);
        int limit = Math.Clamp(request.Limit ?? maxResults, 1, maxResults);
        long from = request.From?.ToUnixTimeMilliseconds() ?? long.MinValue;
        long to = request.To?.ToUnixTimeMilliseconds() ?? long.MaxValue;
        int kindMask = 0;
        if (request.Kind == null || request.Kind.Length == 0) kindMask = AllKinds;
        else foreach (var k in request.Kind) kindMask |= KindBit(k);
        string? appLc = string.IsNullOrWhiteSpace(request.App) ? null : request.App.Trim().ToLowerInvariant();
        string? site = string.IsNullOrWhiteSpace(request.Site) ? null : NormalizeSite(request.Site);
        bool useText = request.Mode != SearchMode.Vector;
        bool useVec = request.Mode != SearchMode.Text && queryVector.Length > 0;

        Task<List<long>>? textTask = useText ? Task.Run(() => FtsSearch(request.Q ?? "", from, to, kindMask, appLc, site)) : null;
        var vecScored = useVec ? VectorSearch(queryVector, from, to, kindMask, appLc, site) : [];
        var vecRanked = vecScored.Select(v => v.Rid).ToList();
        var cosById = new Dictionary<long, float>(vecScored.Count);
        foreach (var (vr, vc) in vecScored) cosById[vr] = vc;
        long tVec = sw.ElapsedTicks;
        var textRanked = textTask?.GetAwaiter().GetResult() ?? [];
        var textSet = new HashSet<long>(textRanked);
        long tText = sw.ElapsedTicks;

        int lists = (useText ? 1 : 0) + (useVec ? 1 : 0);
        var fused = new Dictionary<long, double>();
        for (int i = 0; i < textRanked.Count; i++) fused[textRanked[i]] = fused.GetValueOrDefault(textRanked[i]) + 1.0 / (RrfK + i + 1);
        for (int i = 0; i < vecRanked.Count; i++) fused[vecRanked[i]] = fused.GetValueOrDefault(vecRanked[i]) + 1.0 / (RrfK + i + 1);
        double norm = lists == 0 ? 1 : lists * (1.0 / (RrfK + 1));
        if (useVec && (lists == 1 || textRanked.Count == 0))
        {
            // Vector-only (by mode, or because no text matched): rank-based RRF would report a fixed score for the best
            // hit (1.0, or 0.5 in hybrid mode) even when nothing matches (D11). Report the cosine itself, so a weak
            // match shows a low score.
            fused.Clear();
            foreach (var (rid, cos) in vecScored) fused[rid] = Math.Max(0, cos);
            norm = 1;
        }
        var top = fused.OrderByDescending(kv => kv.Value).ThenByDescending(kv => kv.Key).Take(limit).ToList();

        var hits = new List<SearchHit>(top.Count);
        if (top.Count > 0)
        {
            var meta = new Dictionary<long, (string Id, long Ts, int Kind, string App, string Title, string? Text, bool Kf, int Dur, string Site, string? Replay, int? Offset)>();
            lock (_rl)
            {
                ThrowIfDisposed();
                using var cmd = Cmd(_r, $"""
                    SELECT c.rid, c.id, c.ts, c.kind, c.app, c.title, c.text, c.has_keyframe, c.duration_ms, c.site, r.replay_id, r.offset_ms
                    FROM captures c LEFT JOIN replay_frames r ON r.rid = c.rid WHERE c.rid IN ({InList(top.Select(t => t.Key))}) AND +c.state=1
                    """);
                using var rd = cmd.ExecuteReader();
                while (rd.Read())
                    meta[rd.GetInt64(0)] = (rd.GetString(1), rd.GetInt64(2), rd.GetInt32(3), rd.GetString(4), rd.GetString(5), rd.IsDBNull(6) ? null : rd.GetString(6), rd.GetInt32(7) == 1, rd.GetInt32(8), rd.GetString(9),
                        rd.IsDBNull(10) ? null : rd.GetString(10), rd.IsDBNull(11) ? null : rd.GetInt32(11));
            }
            var tokens = Tokens(request.Q ?? "");
            foreach (var (rid, score) in top)
            {
                if (!meta.TryGetValue(rid, out var m)) continue;
                bool audio = m.Kind == KindAudio;
                var snippet = audio ? Snippet(m.Text ?? "", tokens) : ScreenSnippet(m.Title, m.Text, tokens);
                hits.Add(new SearchHit(m.Id, DateTimeOffset.FromUnixTimeMilliseconds(m.Ts), KindOf(m.Kind), m.App, m.Title,
                    (float)Math.Round(Math.Min(1.0, score / norm), 4), snippet, m.Kf && !audio, audio ? (m.Dur + 500) / 1000 : null,
                    cosById.TryGetValue(rid, out var hc) ? (float)Math.Round(hc, 4) : null, textSet.Contains(rid), Site: m.Site,
                    ReplayId: m.Replay, ReplayOffsetMs: m.Offset));
            }
        }

        long tMeta = sw.ElapsedTicks;
        bool truncated = false;
        if (maxResponseBytes > 0)
        {
            var empty = new SearchResponse(0, true, sw.ElapsedMilliseconds, []);
            long budget = maxResponseBytes - JsonSerializer.SerializeToUtf8Bytes(empty, CoreJson.Default.SearchResponse).Length - 64;
            long used = 0;
            int keep = 0;
            foreach (var h in hits)
            {
                long size = JsonSerializer.SerializeToUtf8Bytes(h, CoreJson.Default.SearchHit).Length + 1;
                if (used + size > budget) break;
                used += size;
                keep++;
            }
            if (keep < hits.Count) { hits.RemoveRange(keep, hits.Count - keep); truncated = true; }
        }
        LastTimings = $"vec {Ms(tVec)} text {Ms(tText)} meta {Ms(tMeta)} total {Ms(sw.ElapsedTicks)}";
        return new SearchResponse(hits.Count, truncated, sw.ElapsedMilliseconds, hits);
    }

    private const int FtsFilteredCandidates = 4000;

    /// <summary>Stage timings of the last search, for benchmarks.</summary>
    internal string? LastTimings { get; private set; }
    internal string? LastVectorTimings { get; private set; }
    private static string Ms(long ticks) => (ticks * 1000.0 / Stopwatch.Frequency).ToString("F1", CultureInfo.InvariantCulture);

    /// <summary>
    /// FTS-driven: rank inside FTS5 first, then apply filters to the ranked ids. Only ready rows are ever in fts,
    /// so no state check is needed. Joining captures inside the MATCH query let SQLite plan from captures and
    /// probe FTS per row (seconds at 250k rows), so it is avoided.
    /// </summary>
    private List<long> FtsSearch(string q, long from, long to, int kindMask, string? appLc, string? site)
    {
        var match = FtsQuery(q);
        if (match == null) return [];
        bool filtered = from != long.MinValue || to != long.MaxValue || kindMask != AllKinds || appLc != null || site != null;
        var ranked = new List<long>();
        lock (_rl)
        {
            ThrowIfDisposed();
            using (var cmd = Cmd(_r, "SELECT rowid FROM fts WHERE fts MATCH $q ORDER BY bm25(fts, 4.0, 2.0, 1.0) LIMIT $n",
                ("$q", match), ("$n", filtered ? FtsFilteredCandidates : FtsCandidates)))
            using (var rd = cmd.ExecuteReader())
                while (rd.Read()) ranked.Add(rd.GetInt64(0));
            if (!filtered || ranked.Count == 0) return ranked;

            var sql = new StringBuilder("SELECT rid FROM captures WHERE rid IN (").Append(InList(ranked)).Append(')');
            var ps = new List<(string, object?)>();
            // Unary + keeps SQLite on the rowid IN list instead of a (state, ts) or app index scan.
            if (from != long.MinValue) { sql.Append(" AND +ts >= $from"); ps.Add(("$from", from)); }
            if (to != long.MaxValue) { sql.Append(" AND +ts < $to"); ps.Add(("$to", to)); }
            if (kindMask != AllKinds)
            {
                var kinds = new List<string>(3);
                if ((kindMask & 1) != 0) kinds.Add("0");
                if ((kindMask & 2) != 0) kinds.Add("1");
                if ((kindMask & 4) != 0) kinds.Add("2");
                sql.Append(" AND +kind IN (").Append(kinds.Count == 0 ? "-1" : string.Join(",", kinds)).Append(')');
            }
            if (appLc != null) { sql.Append(" AND +app_lc = $app"); ps.Add(("$app", appLc)); }
            if (site != null) { sql.Append(" AND +site = $site"); ps.Add(("$site", site)); }
            var keep = new HashSet<long>();
            using (var cmd = Cmd(_r, sql.ToString(), [.. ps]))
            using (var rd = cmd.ExecuteReader())
                while (rd.Read()) keep.Add(rd.GetInt64(0));
            return ranked.Where(keep.Contains).Take(FtsCandidates).ToList();
        }
    }

    private List<(long Rid, float Cos)> VectorSearch(ReadOnlySpan<float> q, long from, long to, int kindMask, string? appLc, string? site)
    {
        int appId = appLc == null ? -1 : _index.AppId(appLc);
        int siteId = site == null ? -1 : _index.SiteId(site);
        var vsw = Stopwatch.StartNew();
        var cands = _index.Scan(q, new VectorFilter(from, to, kindMask, appId, siteId), VectorScanTop);
        LastVectorTimings = $"scan {Ms(vsw.ElapsedTicks)}";
        if (cands.Count == 0) return [];
        var byRid = cands.ToDictionary(c => c.Rid, c => c.Dim);
        var scored = new List<(long Rid, float Cos)>(cands.Count);
        lock (_vl)
        {
            ThrowIfDisposed();
            using var cmd = Cmd(_rv, $"SELECT rid, vec FROM vectors WHERE rid IN ({InList(byRid.Keys.Order())})");
            using var rd = cmd.ExecuteReader();
            while (rd.Read())
            {
                long rid = rd.GetInt64(0);
                var blob = (byte[])rd.GetValue(1);
                var row = MemoryMarshal.Cast<byte, float>(blob);
                int dim = Math.Min(Math.Min(byRid[rid], row.Length), q.Length);
                scored.Add((rid, BinaryVectorIndex.PrefixCosine(q, row, dim)));
            }
        }
        LastVectorTimings += $" rerank-done {Ms(vsw.ElapsedTicks)}";
        return scored.OrderByDescending(s => s.Cos).Take(VectorKeep).ToList();
    }

    /// <summary>Lower-case host without "www.", at most 253 characters; anything else becomes empty.</summary>
    public static string NormalizeSite(string? site)
    {
        if (string.IsNullOrWhiteSpace(site)) return "";
        var s = site.Trim().ToLowerInvariant();
        if (s.StartsWith("www.", StringComparison.Ordinal)) s = s[4..];
        return s.Length is > 0 and <= 253 && s.All(c => char.IsAsciiLetterOrDigit(c) || c is '.' or '-' || c > 127) ? s : "";
    }

    // ---------------------------------------------------------------- facets and search history

    public FacetsReply Facets(DateTimeOffset? from, DateTimeOffset? to, int limit)
    {
        limit = Math.Clamp(limit, 1, 200);
        long f = from?.ToUnixTimeMilliseconds() ?? long.MinValue, t = to?.ToUnixTimeMilliseconds() ?? long.MaxValue;
        lock (_rl)
        {
            ThrowIfDisposed();
            List<FacetCount> Read(string sql)
            {
                var list = new List<FacetCount>();
                using var cmd = Cmd(_r, sql, ("$from", f), ("$to", t), ("$n", limit));
                using var rd = cmd.ExecuteReader();
                while (rd.Read()) list.Add(new FacetCount(rd.GetString(0), rd.GetInt32(1)));
                return list;
            }
            var apps = Read("""
                SELECT max(app), count(*) AS n FROM captures WHERE state = 1 AND ts >= $from AND ts < $to AND app <> ''
                GROUP BY app_lc ORDER BY n DESC, 1 LIMIT $n
                """);
            var sites = Read("""
                SELECT site, count(*) AS n FROM captures WHERE state = 1 AND ts >= $from AND ts < $to AND site <> ''
                GROUP BY site ORDER BY n DESC, 1 LIMIT $n
                """);
            return new FacetsReply(apps, sites);
        }
    }

    public IReadOnlyList<HistoryEntry> History(int limit)
    {
        limit = Math.Clamp(limit, 1, 1000);
        lock (_rl)
        {
            ThrowIfDisposed();
            using var cmd = Cmd(_r, "SELECT q, ts FROM search_history ORDER BY ts DESC LIMIT $n", ("$n", limit));
            using var rd = cmd.ExecuteReader();
            var list = new List<HistoryEntry>();
            while (rd.Read()) list.Add(new HistoryEntry(rd.GetString(0), DateTimeOffset.FromUnixTimeMilliseconds(rd.GetInt64(1))));
            return list;
        }
    }

    public void AddHistory(string q, DateTimeOffset ts, int max)
    {
        q = (q ?? "").Trim();
        if (q.Length is 0 or > 1024) return;
        lock (_wl)
        {
            ThrowIfDisposed();
            Exec(_w, "INSERT INTO search_history(q, ts) VALUES($q, $ts) ON CONFLICT(q) DO UPDATE SET ts=excluded.ts",
                ("$q", q), ("$ts", ts.ToUnixTimeMilliseconds()));
            Exec(_w, "DELETE FROM search_history WHERE q NOT IN (SELECT q FROM search_history ORDER BY ts DESC LIMIT $n)", ("$n", Math.Max(1, max)));
        }
    }

    public int DeleteHistory(string? q)
    {
        lock (_wl)
        {
            ThrowIfDisposed();
            using var cmd = q == null ? Cmd(_w, "DELETE FROM search_history") : Cmd(_w, "DELETE FROM search_history WHERE q=$q", ("$q", q.Trim()));
            int n = cmd.ExecuteNonQuery();
            if (q == null) CheckpointLocked();
            return n;
        }
    }

    /// <summary>Words a saved search would use for an app or site scope: "chrome.exe" gives chrome.exe and chrome;
    /// "www.pornhub.co.uk" gives pornhub.co.uk and pornhub. Words under 3 characters are dropped (too broad).</summary>
    internal static List<string> HistoryNeedles(PurgeScope scope)
    {
        var needles = new List<string>();
        void Add(string? w) { w = w?.Trim(); if (w is { Length: >= 3 } && !needles.Contains(w, StringComparer.OrdinalIgnoreCase)) needles.Add(w); }
        if (scope.Kind == "app" && !string.IsNullOrWhiteSpace(scope.App))
        {
            var app = scope.App.Trim();
            Add(app);
            if (app.EndsWith(".exe", StringComparison.OrdinalIgnoreCase)) Add(app[..^4]);
        }
        else if (scope.Kind == "site" && NormalizeSite(scope.Site) is { Length: > 0 } host)
        {
            if (host.StartsWith("www.", StringComparison.Ordinal)) host = host[4..];
            Add(host);
            var labels = host.Split('.', StringSplitOptions.RemoveEmptyEntries);
            // Main label: the one before the suffix ("co.uk", "com.au" count as one suffix).
            int i = labels.Length - 2;
            if (labels.Length >= 3 && labels[^2] is "co" or "com" or "net" or "org" or "gov" or "edu" or "ac") i--;
            if (i >= 0) Add(labels[i]);
        }
        return needles;
    }

    /// <summary>Deletes saved searches containing any needle (case-insensitive). Caller holds _wl.</summary>
    private void DeleteHistoryContainingLocked(List<string> needles)
    {
        if (needles.Count == 0) return;
        var drop = new List<string>();
        using (var cmd = Cmd(_w, "SELECT q FROM search_history"))
        using (var rd = cmd.ExecuteReader())
            while (rd.Read())
            {
                var q = rd.GetString(0);
                if (needles.Any(n => q.Contains(n, StringComparison.OrdinalIgnoreCase))) drop.Add(q);
            }
        foreach (var q in drop) Exec(_w, "DELETE FROM search_history WHERE q=$q", ("$q", q));
    }

    internal static string Cut(string s, int max) => s.Length <= max ? s : s[..(max - 1)] + "…";

    /// <summary>Screen hit: the title, unless a query token appears only in the OCR text; then the text around it.</summary>
    internal static string ScreenSnippet(string title, string? text, IReadOnlyList<string> tokens)
    {
        if (string.IsNullOrEmpty(text) || tokens.Count == 0) return Cut(title, SnippetMax);
        bool inTitle = tokens.Any(t => title.Contains(t, StringComparison.OrdinalIgnoreCase));
        bool inText = tokens.Any(t => text.Contains(t, StringComparison.OrdinalIgnoreCase));
        return inText && !inTitle ? Snippet(text, tokens) : Cut(title, SnippetMax);
    }

    internal static string Snippet(string text, IReadOnlyList<string> tokens)
    {
        if (text.Length <= SnippetMax) return text;
        int at = -1;
        foreach (var t in tokens)
        {
            at = text.IndexOf(t, StringComparison.OrdinalIgnoreCase);
            if (at >= 0) break;
        }
        if (at < 0) return Cut(text, SnippetMax);
        int start = Math.Max(0, at - 100);
        int len = Math.Min(SnippetMax - 2, text.Length - start);
        var core = text.Substring(start, len);
        var s = (start > 0 ? "…" : "") + core + (start + len < text.Length ? "…" : "");
        return s.Length <= SnippetMax ? s : s[..SnippetMax];
    }

    // ---------------------------------------------------------------- audit and stats

    public void AppendAudit(AuditEntry e)
    {
        ArgumentNullException.ThrowIfNull(e);
        lock (_wl)
        {
            ThrowIfDisposed();
            Exec(_w, "INSERT INTO audit(ts, pipe, pid, image, verb, count, sha) VALUES($ts, $p, $pid, $img, $v, $n, $sha)",
                ("$ts", e.Ts.ToUnixTimeMilliseconds()), ("$p", e.Pipe ?? ""), ("$pid", e.ClientPid), ("$img", e.ClientImage ?? ""),
                ("$v", e.Verb ?? ""), ("$n", e.ResultCount), ("$sha", e.QuerySha256 ?? ""));
        }
    }

    public IReadOnlyList<AuditEntry> ReadAudit(int limit)
    {
        limit = Math.Clamp(limit, 1, 10_000);
        lock (_rl)
        {
            ThrowIfDisposed();
            using var cmd = Cmd(_r, "SELECT ts, pipe, pid, image, verb, count, sha FROM audit ORDER BY seq DESC LIMIT $n", ("$n", limit));
            using var rd = cmd.ExecuteReader();
            var list = new List<AuditEntry>();
            while (rd.Read())
                list.Add(new AuditEntry(DateTimeOffset.FromUnixTimeMilliseconds(rd.GetInt64(0)), rd.GetString(1), rd.GetInt32(2), rd.GetString(3), rd.GetString(4), rd.GetInt32(5), rd.GetString(6)));
            return list;
        }
    }

    private static long FileSize(string p) { try { return File.Exists(p) ? new FileInfo(p).Length : 0; } catch { return 0; } }

    private long KeyframeBytes()
    {
        long t = 0;
        foreach (var m in KfMonths()) { var p = KfPath(m); t += FileSize(p) + FileSize(p + "-wal"); }
        return t;
    }

    public StoreStats Stats()
    {
        lock (_rl)
        {
            ThrowIfDisposed();
            long rows = (long)(Scalar(_r, "SELECT count(*) FROM captures WHERE state=1") ?? 0L);
            var oldest = Scalar(_r, "SELECT min(ts) FROM captures WHERE state=1") as long?;
            var newest = Scalar(_r, "SELECT max(ts) FROM captures WHERE state=1") as long?;
            long pending = (long)(Scalar(_r, "SELECT count(*) FROM jobs") ?? 0L);
            long vectors = (long)(Scalar(_r, "SELECT count(*) FROM vectors") ?? 0L);
            var db = DataPaths.StoreDb(_home);
            long kfBytes = KeyframeBytes();
            return new StoreStats(rows, FileSize(db) + FileSize(db + "-wal") + kfBytes, kfBytes,
                oldest is long o ? DateTimeOffset.FromUnixTimeMilliseconds(o) : null,
                newest is long n ? DateTimeOffset.FromUnixTimeMilliseconds(n) : null,
                (int)pending, vectors);
        }
    }

    // ---------------------------------------------------------------- maintenance

    /// <summary>
    /// Hourly maintenance in short _wl slices (one retention chunk, one thinning day, one month file, ...), so capture
    /// inserts, keyframe reads (thumbnails, detail pane, chat images) and a panic purge wait for one slice, not for the
    /// whole run. Purge and keyframe reads enter with priority: maintenance does not start its next slice while one waits.
    /// The retention notice is raised even when a step throws (a damaged month file, VACUUM out of memory or disk), so
    /// the RAM copies of rows already deleted (thumbnails, chats, approvals, jobs) are dropped (B11).
    /// </summary>
    public void RunMaintenance(Settings settings)
    {
        ArgumentNullException.ThrowIfNull(settings);
        try { RunMaintenanceSteps(settings); }
        finally
        {
            PurgeNotice? notice = null;
            try { notice = MaintenanceSlice(() => TakeNoticeLocked("retention")); }
            catch (Exception ex) { Log.Warn("maintenance notice", ex); }
            Raise(notice);
        }
    }

    private void RunMaintenanceSteps(Settings settings)
    {
        var sw = Stopwatch.StartNew();
        var now = DateTimeOffset.UtcNow;
        long cutoff = now.AddDays(-settings.Retention.Days).ToUnixTimeMilliseconds();
        string cutoffMonth = Month(cutoff);

        // 1. Retention: rows older than retention.days, 5000 per slice.
        int expired = 0;
        while (MaintenanceSlice(() =>
        {
            var rids = new List<long>();
            using (var cmd = Cmd(_w, "SELECT rid FROM captures WHERE ts < $c LIMIT 5000", ("$c", cutoff)))
            using (var rd = cmd.ExecuteReader())
                while (rd.Read()) rids.Add(rd.GetInt64(0));
            if (rids.Count == 0) return false;
            expired += DeleteRowsLocked(rids);
            return true;
        })) { }
        MaintenanceSlice(() =>
        {
            Exec(_w, "DELETE FROM audit WHERE ts < $c", ("$c", cutoff));
            // Searches and chats are memory too: retention.days covers them (a chat goes once its first message expires).
            Exec(_w, "DELETE FROM search_history WHERE ts < $c", ("$c", cutoff));
            DeleteChatsOlderLocked(cutoff);
            return true;
        });

        // 2. Whole keyframe files from months before the cutoff month are empty now: delete the files.
        var months = MaintenanceSlice(KfMonths);
        foreach (var m in months.Where(m => string.CompareOrdinal(m, cutoffMonth) < 0))
            MaintenanceSlice(() =>
            {
                if ((long)(Scalar(_w, "SELECT count(*) FROM captures WHERE kf_month=$m AND has_keyframe=1", ("$m", m)) ?? 0L) == 0)
                    DeleteKfFile(m);
                return true;
            });

        // 2b. Once per open: keyframe images whose row is gone. A crash between the store.db commit and the month-file
        //     delete of a row delete leaves them behind (B13); nothing else would remove them before their month expires.
        if (!_orphansSwept)
        {
            int orphans = 0;
            foreach (var m in MaintenanceSlice(KfMonths))
                MaintenanceSlice(() => { orphans += DeleteOrphanKeyframesLocked(m); return true; });
            _orphansSwept = true;
            if (orphans > 0) Log.Info($"maintenance: {orphans} orphan keyframes deleted");
        }

        // 3. Progressive thinning (retention.progressive*), one day per slice. storage.maxGb and storage.minFreeGb are
        //    the agent's storage guard (DeleteForSpace).
        int thinned = 0;
        if (MaintenanceSlice(() => ThinDaysLocked(settings.Retention, now, cutoff)) is { } days)
            for (long day = days.First; day <= days.Last; day++)
            {
                long d = day;
                MaintenanceSlice(() => { thinned += ThinDayLocked(settings.Retention, now, cutoff, d); return true; });
            }
        if (thinned > 0) Log.Info($"maintenance: {thinned} rows thinned");
        foreach (var m in MaintenanceSlice(KfMonths))
            // A legacy rebuild pauses for priority callers (S3) and goes on in the next slice.
            while (MaintenanceSlice(() => ReleaseFreePagesLocked(m, vacuumLegacy: true))) { }

        MaintenanceSlice(() =>
        {
            CheckpointLocked();
            lock (_rl)
            {
                long free = (long)(Scalar(_w, "PRAGMA freelist_count") ?? 0L);
                long pages = (long)(Scalar(_w, "PRAGMA page_count") ?? 0L);
                long pageSize = (long)(Scalar(_w, "PRAGMA page_size") ?? 4096L);
                if (pages > 0 && free * 4 > pages && free * pageSize > 16L * 1024 * 1024)
                {
                    // VACUUM builds its copy in RAM (SQLITE_TEMP_STORE=2; a file temp_store is not encrypted): about the
                    // live size. Above the budget the free pages stay for new rows to reuse (B4).
                    long live = (pages - free) * pageSize;
                    if (live <= VacuumRamBudgetBytes)
                    {
                        Exec(_w, "VACUUM");
                        Log.Info("store.db vacuumed");
                    }
                    else Log.Info($"store.db not vacuumed: {live / (1024 * 1024)} MiB live is over the RAM budget");
                }
            }
            try { Exec(_w, "PRAGMA optimize"); } catch { }
            return true;
        });
        Log.Info($"maintenance: {expired} rows expired in {sw.ElapsedMilliseconds} ms");
    }

    /// <summary>Largest live store.db that the maintenance VACUUMs (128 MiB; tests lower it).</summary>
    internal long VacuumRamBudgetBytes { get; set; } = 128L * 1024 * 1024;

    /// <summary>Count of callers waiting for _wl with priority (purge, keyframe reads).</summary>
    private int _priorityWaiters;
    private bool _orphansSwept;

    /// <summary>Takes _wl for one maintenance step, after any priority caller that is waiting got its turn.</summary>
    private T MaintenanceSlice<T>(Func<T> step)
    {
        var wait = Stopwatch.StartNew();
        while (Volatile.Read(ref _priorityWaiters) > 0 && wait.ElapsedMilliseconds < 30_000) Thread.Sleep(2);
        lock (_wl)
        {
            ThrowIfDisposed();
            return step();
        }
    }

    /// <summary>_wl for a caller that must not queue behind maintenance (panic purge, keyframe reads).</summary>
    private Lock.Scope EnterWritePriority()
    {
        Interlocked.Increment(ref _priorityWaiters);
        try { return _wl.EnterScope(); }
        finally { Interlocked.Decrement(ref _priorityWaiters); }
    }

    /// <summary>Deletes keyframe images of one month file whose capture row no longer names that month. Caller holds _wl.</summary>
    private int DeleteOrphanKeyframesLocked(string month)
    {
        if (Kf(month, false) is not { } kc) return 0;
        var rows = new HashSet<long>();
        using (var cmd = Cmd(_w, "SELECT rid FROM captures WHERE kf_month=$m", ("$m", month)))
        using (var rd = cmd.ExecuteReader())
            while (rd.Read()) rows.Add(rd.GetInt64(0));
        var orphans = new List<long>();
        using (var cmd = Cmd(kc, "SELECT rid FROM kf"))
        using (var rd = cmd.ExecuteReader())
            while (rd.Read()) { long rid = rd.GetInt64(0); if (!rows.Contains(rid)) orphans.Add(rid); }
        for (int off = 0; off < orphans.Count; off += 500)
            Exec(kc, $"DELETE FROM kf WHERE rid IN ({InList(orphans.Skip(off).Take(500).ToList())})");
        return orphans.Count;
    }

    // ---------------------------------------------------------------- test hooks

    /// <summary>Tests: keyframe images stored across all month files.</summary>
    internal long KeyframeCountForTest()
    {
        lock (_wl)
        {
            long n = 0;
            foreach (var m in KfMonths()) if (Kf(m, false) is { } c) n += (long)(Scalar(c, "SELECT count(*) FROM kf") ?? 0L);
            return n;
        }
    }

    /// <summary>Tests: the state a crash between the two commits of a row delete leaves (row gone, image kept).</summary>
    internal void DeleteRowKeepImageForTest(string id)
    {
        lock (_wl)
        {
            if (Rid(id) is not long rid) return;
            Exec(_w, "DELETE FROM vectors WHERE rid=$r", ("$r", rid));
            Exec(_w, "DELETE FROM jobs WHERE rid=$r", ("$r", rid));
            Exec(_w, "DELETE FROM captures WHERE rid=$r", ("$r", rid));
            _index.Remove(rid);
        }
    }

    /// <summary>
    /// Tests and benchmarks only: inserts ready screen rows with vectors and FTS entries in one transaction,
    /// without keyframes, and adds them to the index.
    /// </summary>
    internal void InsertSynthetic(IEnumerable<(DateTimeOffset Ts, string App, string Title, float[] Vector)> rows, EmbeddingInfo info)
    {
        lock (_wl)
        {
            ThrowIfDisposed();
            if (_space == null) WriteSpace(info);
            using var tx = BeginWrite();
            using var ins = Cmd(_w, """
                INSERT INTO captures(id, ts, kind, app, app_lc, exe, title, monitor, width, height, dhash, duration_ms, state, has_keyframe, keep_keyframe, kf_month)
                VALUES($id, $ts, 0, $app, $applc, '', $title, 0, 0, 0, 0, 0, 1, 0, 1, NULL) RETURNING rid
                """, ("$id", ""), ("$ts", 0L), ("$app", ""), ("$applc", ""), ("$title", ""));
            using var vec = Cmd(_w, "INSERT INTO vectors(rid, model_id, revision, dim, vec, bin) VALUES($rid, $m, $r, $d, $v, $b)",
                ("$rid", 0L), ("$m", info.ModelId), ("$r", info.Revision), ("$d", info.Dim), ("$v", Array.Empty<byte>()), ("$b", Array.Empty<byte>()));
            using var fts = Cmd(_w, "INSERT INTO fts(rowid, title, app, text) VALUES($rid, $t, $a, NULL)", ("$rid", 0L), ("$t", ""), ("$a", ""));
            var added = new List<(long, float[], long, string)>();
            foreach (var (ts, app, title, vector) in rows)
            {
                var v = Normalize(vector);
                long t = ts.ToUnixTimeMilliseconds();
                ins.Parameters["$id"].Value = Ids.New('s', ts);
                ins.Parameters["$ts"].Value = t;
                ins.Parameters["$app"].Value = app;
                ins.Parameters["$applc"].Value = app.ToLowerInvariant();
                ins.Parameters["$title"].Value = title;
                long rid = (long)ins.ExecuteScalar()!;
                vec.Parameters["$rid"].Value = rid;
                vec.Parameters["$v"].Value = ToBytes(v);
                vec.Parameters["$b"].Value = BinaryVectorIndex.SignBitsBytes(v);
                vec.ExecuteNonQuery();
                fts.Parameters["$rid"].Value = rid;
                fts.Parameters["$t"].Value = title;
                fts.Parameters["$a"].Value = app;
                fts.ExecuteNonQuery();
                if (InSpace(info.ModelId, info.Revision)) _index.Upsert(rid, v, t, KindScreen, app.ToLowerInvariant());
            }
            tx.Commit();
        }
    }

    // ---------------------------------------------------------------- lifecycle

    /// <summary>Closes every connection and deletes store.db and all keyframe files (purge all). The instance is unusable afterwards.</summary>
    public void DeleteAllFiles()
    {
        lock (_wl)
        lock (_rl)
        {
            CloseLocked();
            DeleteFiles(_home);
        }
    }

    /// <summary>Deletes store.db (+ -wal, -shm) and every keyframe file of a data folder without opening them; purge all
    /// uses it when the store never opened (data key missing or wrong).</summary>
    public static void DeleteFiles(string home)
    {
        var db = DataPaths.StoreDb(home);
        foreach (var p in new[] { db, db + "-wal", db + "-shm" })
            try { if (File.Exists(p)) File.Delete(p); } catch (Exception ex) { Log.Error("store file delete failed", ex); }
        var kfDir = DataPaths.KeyframesDir(home);
        if (Directory.Exists(kfDir))
            foreach (var f in Directory.GetFiles(kfDir))
                try { File.Delete(f); } catch (Exception ex) { Log.Error("keyframe file delete failed", ex); }
    }

    private void CloseLocked()
    {
        if (_disposed) return;
        _disposed = true;
        try { Exec(_w, "PRAGMA wal_checkpoint(TRUNCATE)"); } catch { }
        AbortRebuildLocked(null);   // a paused month rebuild: its temporary file must not outlive the store (purge all)
        foreach (var c in _kf.Values) { try { Exec(c, "PRAGMA wal_checkpoint(TRUNCATE)"); } catch { } c.Dispose(); }
        _kf.Clear();
        lock (_vl) _rv.Dispose();
        _r.Dispose();
        _w.Dispose();
        SqliteConnection.ClearAllPools();
        _index.Clear();
        _inflight.Clear();
    }

    public void Dispose()
    {
        lock (_wl)
        lock (_rl)
            CloseLocked();
    }
}
