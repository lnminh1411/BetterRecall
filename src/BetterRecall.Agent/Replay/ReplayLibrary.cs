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
    public const string TimeFormat = "yyyyMMdd'T'HHmmss'Z'";
    private readonly Dictionary<string, (long Length, int DurationMs)> _durations = new();

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
            if (!DateTime.TryParseExact(id, TimeFormat, CultureInfo.InvariantCulture, DateTimeStyles.AdjustToUniversal | DateTimeStyles.AssumeUniversal, out var ts)) continue;
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
        var clip = Find(id) ?? throw new FileNotFoundException("Replay not found.");
        var full = Path.GetFullPath(target);
        Directory.CreateDirectory(Path.GetDirectoryName(full)!);
        await using var src = clip.Encrypted
            ? (cipher ?? throw new InvalidOperationException("Store is locked.")).OpenRead(clip.Path)
            : File.OpenRead(clip.Path);
        await using var dst = new FileStream(full, FileMode.Create, FileAccess.Write, FileShare.None);
        await src.CopyToAsync(dst, ct).ConfigureAwait(false);
    }

    public void Delete(string id)
    {
        var clip = Find(id) ?? throw new FileNotFoundException("Replay not found.");
        File.Delete(clip.Path);
        lock (_durations) _durations.Remove(clip.Path);
    }
}
