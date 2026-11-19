// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Buffers.Binary;
using System.Globalization;
using BetterRecall.Contracts;

namespace BetterRecall.Agent.Replay;

/// <summary>
/// Saved replay clips: replays\&lt;ts&gt;.mp4.enc (AES-256-GCM via IBlobCipher) or plain &lt;ts&gt;.mp4 in replay.exportDir.
/// The id is the timestamp stem. Duration is read from the mvhd box at the head of the file.
/// </summary>
internal sealed class ReplayLibrary(string home, Func<Settings> settings, IBlobCipher? cipher)
{
    /// <summary>Id of a new clip: millisecond resolution, so two saves in one second do not share a file (A20).</summary>
    public const string TimeFormat = "yyyyMMdd'T'HHmmssfff'Z'";
    /// <summary>Ids written before 0.2.1 (one-second resolution); still listed, exported and deleted.</summary>
    public const string LegacyTimeFormat = "yyyyMMdd'T'HHmmss'Z'";
    private static readonly string[] IdFormats = [TimeFormat, LegacyTimeFormat];
    private readonly Dictionary<string, (long Length, int DurationMs)> _durations = new();
    private readonly HashSet<string> _reserved = new();
    private int _purgeEpoch;
    private CancellationTokenSource _purgeCts = new();
    // Held by the purge calls and around each save's or export's last check plus rename: once OnPurgeAll or
    // DiscardSavesInFlight returns, a rename either finished before it or sees the purge.
    private readonly Lock _purgeLock = new();

    /// <summary>Read at the start of WriteClipAsync; bumped by purge all and by a user purge that reaches the replay
    /// ring. A save drops its clip when it changed (C1, BC-S1).</summary>
    public int PurgeEpoch => Volatile.Read(ref _purgeEpoch);

    /// <summary>Core's "purge all is running" (set before its notice, cleared before the new store opens). A save or
    /// export that would start inside it is refused: it could open a clip, or write one, under the old key.</summary>
    public Func<bool>? PurgingAll { get; set; }

    /// <summary>Agent's read gate is closed: decrypted replay exports abort and delete their partial file (sweep 5 B1).</summary>
    public Func<bool>? Locked { get; set; }

    private bool Purging => PurgingAll?.Invoke() == true;
    private bool IsLocked => Locked?.Invoke() == true;

    /// <summary>Purge all: running decrypted exports stop and delete their output; saves in flight see a new epoch.</summary>
    public void OnPurgeAll()
    {
        lock (_purgeLock)
        {
            Interlocked.Increment(ref _purgeEpoch);
            var old = _purgeCts;
            _purgeCts = new CancellationTokenSource();
            try { old.Cancel(); } catch (AggregateException ex) { Util.Log.Warn("replay export cancel", ex); }
        }
    }

    /// <summary>A user purge reached the replay ring (app, site, a window near now, panic): a clip being written holds
    /// the purged video, so it is discarded as under purge all. Exports of saved clips go on (saved clips are exports,
    /// sweep 3 B5).</summary>
    public void DiscardSavesInFlight()
    {
        lock (_purgeLock) Interlocked.Increment(ref _purgeEpoch);
    }

    public string EncryptedDir => DataPaths.ReplaysDir(home);

    public string PlainDir
    {
        get
        {
            var d = settings().Replay.ExportDir;
            return string.IsNullOrWhiteSpace(d) ? DataPaths.ReplaysDir(home) : d;
        }
    }

    public static string NewId(DateTimeOffset ts) => ts.UtcDateTime.ToString(TimeFormat, CultureInfo.InvariantCulture);

    public static bool TryParseId(string id, out DateTime ts) =>
        DateTime.TryParseExact(id, IdFormats, CultureInfo.InvariantCulture, DateTimeStyles.AdjustToUniversal | DateTimeStyles.AssumeUniversal, out ts);

    /// <summary>An id for a clip saved at ts that no clip, partial file or save in progress uses: one millisecond later
    /// per clash. Reserved for this process; the caller saves one clip per id.</summary>
    public string UniqueId(DateTimeOffset ts)
    {
        lock (_reserved)
        {
            for (int i = 0; ; i++)
            {
                var id = NewId(ts + TimeSpan.FromMilliseconds(i));
                if (_reserved.Contains(id) || Taken(id)) continue;
                _reserved.Add(id);
                return id;
            }
        }
    }

    private bool Taken(string id)
    {
        foreach (var p in new[] { Path.Combine(EncryptedDir, id + ".mp4.enc"), Path.Combine(PlainDir, id + ".mp4") })
            if (File.Exists(p) || File.Exists(p + ".partial")) return true;
        return false;
    }

    /// <summary>Deletes *.partial files left by a failed or interrupted save (start only: no save is running yet).
    /// In plain-export mode they hold unencrypted video. Returns how many were deleted.</summary>
    public int DeletePartials()
    {
        int n = 0;
        foreach (var (dir, pattern) in new[] { (EncryptedDir, "*.mp4.enc.partial"), (PlainDir, "*.mp4.partial") })
        {
            try
            {
                if (!Directory.Exists(dir)) continue;
                foreach (var f in Directory.EnumerateFiles(dir, pattern))
                {
                    try { File.Delete(f); n++; }
                    catch (Exception ex) { Util.Log.Warn($"replay partial {Path.GetFileName(f)}", ex); }
                }
            }
            catch (Exception ex) { Util.Log.Warn("replay partial cleanup", ex); }
        }
        if (n > 0) Util.Log.Info($"replays: {n} unfinished save(s) deleted");
        return n;
    }

    /// <summary>Writes a clip through save into path + ".partial" (encrypted with the data key, or plain), then renames
    /// it to path. A failed save never leaves the partial behind (in plain-export mode it is not encrypted). When purge
    /// all, or a user purge that reaches the ring, runs while the clip is written, the clip holds purged video (plain,
    /// or under the destroyed key): it is deleted instead of renamed (C1, BC-S1). Returns save's result (the clip
    /// duration in ms).</summary>
    public async Task<int> WriteClipAsync(string path, bool encrypted, Func<Stream, int> save, CancellationToken ct)
    {
        var partial = path + ".partial";
        int epoch = PurgeEpoch;   // before the PurgingAll check: Core sets the flag before its notice bumps the epoch
        if (Purging) throw new InvalidOperationException("Purge all is running.");
        try
        {
            int ms = await Task.Run(() =>
            {
                using var stream = encrypted
                    ? (cipher ?? throw new InvalidOperationException("Store is locked.")).OpenWrite(partial)
                    : new FileStream(partial, FileMode.Create, FileAccess.Write, FileShare.None);
                return save(stream);
            }, ct).ConfigureAwait(false);
            lock (_purgeLock)
            {
                if (PurgeEpoch != epoch || Purging) throw new InvalidOperationException("A purge ran during the replay save; the clip was discarded.");
                File.Move(partial, path, overwrite: false);
            }
            return ms;
        }
        catch
        {
            try { File.Delete(partial); } catch (Exception ex) { Util.Log.Warn("replay partial delete", ex); }
            throw;
        }
    }

    public (string Path, bool Encrypted) TargetFor(string id)
    {
        bool enc = settings().Replay.EncryptExports;
        return enc ? (Path.Combine(EncryptedDir, id + ".mp4.enc"), true) : (Path.Combine(PlainDir, id + ".mp4"), false);
    }

    public IReadOnlyList<ReplayClip> List()
    {
        var list = new List<ReplayClip>();
        Add(list, EncryptedDir, "*.mp4.enc", true);
        Add(list, PlainDir, "*.mp4", false);
        return list.OrderByDescending(c => c.Ts).ToList();
    }

    private void Add(List<ReplayClip> list, string dir, string pattern, bool encrypted)
    {
        if (!Directory.Exists(dir)) return;
        foreach (var path in Directory.EnumerateFiles(dir, pattern))
        {
            var name = Path.GetFileName(path);
            var id = encrypted ? name[..^".mp4.enc".Length] : name[..^".mp4".Length];
            if (!TryParseId(id, out var ts)) continue;
            if (list.Any(c => c.Id == id)) continue;
            long len = new FileInfo(path).Length;
            list.Add(new ReplayClip(id, new DateTimeOffset(ts, TimeSpan.Zero), Duration(path, len, encrypted), len, encrypted, path));
        }
    }

    public ReplayClip? Find(string id) => List().FirstOrDefault(c => c.Id == id);

    private int Duration(string path, long length, bool encrypted)
    {
        lock (_durations)
            if (_durations.TryGetValue(path, out var d) && d.Length == length) return d.DurationMs;
        int ms = 0;
        try
        {
            using var s = encrypted ? cipher?.OpenRead(path) : File.OpenRead(path);
            if (s is not null) ms = ReadMvhdDurationMs(s);
        }
        catch { }
        lock (_durations) _durations[path] = (length, ms);
        return ms;
    }

    /// <summary>Walks top-level boxes to moov/mvhd. Reads at most the first 64 KiB.</summary>
    public static int ReadMvhdDurationMs(Stream s)
    {
        var buf = new byte[65536];
        int n = 0, r;
        while (n < buf.Length && (r = s.Read(buf, n, buf.Length - n)) > 0) n += r;
        var span = buf.AsSpan(0, n);
        int pos = 0;
        while (pos + 8 <= span.Length)
        {
            uint size = BinaryPrimitives.ReadUInt32BigEndian(span[pos..]);
            var type = System.Text.Encoding.ASCII.GetString(span.Slice(pos + 4, 4));
            if (size < 8) break;
            if (type == "moov")
            {
                int inner = pos + 8;
                int end = (int)Math.Min(span.Length, pos + size);
                while (inner + 8 <= end)
                {
                    uint isz = BinaryPrimitives.ReadUInt32BigEndian(span[inner..]);
                    var itype = System.Text.Encoding.ASCII.GetString(span.Slice(inner + 4, 4));
                    if (isz < 8) break;
                    if (itype == "mvhd" && inner + 28 <= end)
                    {
                        byte version = span[inner + 8];
                        if (version == 0)
                        {
                            uint timescale = BinaryPrimitives.ReadUInt32BigEndian(span[(inner + 20)..]);
                            uint duration = BinaryPrimitives.ReadUInt32BigEndian(span[(inner + 24)..]);
                            return timescale == 0 ? 0 : (int)(duration * 1000L / timescale);
                        }
                        uint ts1 = BinaryPrimitives.ReadUInt32BigEndian(span[(inner + 28)..]);
                        ulong dur1 = BinaryPrimitives.ReadUInt64BigEndian(span[(inner + 32)..]);
                        return ts1 == 0 ? 0 : (int)((long)dur1 * 1000 / ts1);
                    }
                    inner += (int)isz;
                }
                return 0;
            }
            pos += (int)size;
        }
        return 0;
    }

    public async Task ExportAsync(string id, string target, CancellationToken ct)
    {
        CancellationToken purge;
        lock (_purgeLock) purge = _purgeCts.Token;
        if (Purging) throw new FileNotFoundException("Replay not found.");   // the clip is being deleted
        if (IsLocked) throw new InvalidOperationException("Store is locked.");
        var clip = Find(id) ?? throw new FileNotFoundException("Replay not found.");
        var full = Path.GetFullPath(target);
        Directory.CreateDirectory(Path.GetDirectoryName(full)!);
        // Written to a .partial listed in the export journal, renamed at the end: a killed agent leaves no truncated but
        // playable decrypted clip under the user's name, and the next start deletes the partial (C3).
        var partial = full + ".partial";
        bool created = false;
        using var linked = CancellationTokenSource.CreateLinkedTokenSource(ct, purge);
        try
        {
            await using (var src = clip.Encrypted
                ? (cipher ?? throw new InvalidOperationException("Store is locked.")).OpenRead(clip.Path)
                : File.OpenRead(clip.Path))
            {
                // Purge all may have started while the source opened (an open encrypted clip keeps the old key).
                if (Purging) throw new FileNotFoundException("Replay not found.");
                if (IsLocked) throw new InvalidOperationException("Store is locked.");
                BetterRecall.Core.Store.ExportJournal.Add(home, partial);
                FileStream dst;
                try { dst = new FileStream(partial, FileMode.Create, FileAccess.Write, FileShare.None); }
                catch { BetterRecall.Core.Store.ExportJournal.Forget(home, partial); throw; }
                created = true;
                await using (dst) await src.CopyToAsync(dst, linked.Token).ConfigureAwait(false);
            }
            lock (_purgeLock)
            {
                linked.Token.ThrowIfCancellationRequested();
                if (IsLocked) throw new InvalidOperationException("Store is locked.");
                File.Move(partial, full, overwrite: true);
            }
            BetterRecall.Core.Store.ExportJournal.Remove(home, partial);
            if (IsLocked)
            {
                try { File.Delete(full); } catch { }
                throw new InvalidOperationException("Store is locked.");
            }
        }
        catch (Exception ex)
        {
            // Cancelled or failed: do not leave a partial decrypted copy behind (only ours: a file of that name that was
            // there before stays).
            if (created)
            {
                try { File.Delete(partial); } catch { }
                BetterRecall.Core.Store.ExportJournal.Remove(home, partial);
            }
            // Purge all during the export (C1): the clip is gone, so is the copy.
            if (purge.IsCancellationRequested && !ct.IsCancellationRequested && ex is OperationCanceledException)
            {
                Util.Log.Info("replay export aborted: purge all ran during it");
                throw new FileNotFoundException("Replay not found.");
            }
            throw;
        }
    }

    public void Delete(string id)
    {
        var clip = Find(id) ?? throw new FileNotFoundException("Replay not found.");
        File.Delete(clip.Path);
        lock (_durations) _durations.Remove(clip.Path);
    }
}
