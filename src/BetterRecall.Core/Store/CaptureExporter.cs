// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Globalization;
using System.IO.Compression;
using System.Text.Json;
using BetterRecall.Contracts;

namespace BetterRecall.Core.Store;

/// <summary>
/// export verb (v0.2 batch 2): searchable captures in [from, to), optionally of one app, written as a zip of decrypted
/// keyframes (images\&lt;local time&gt;_&lt;id&gt;.&lt;ext&gt;) plus captures.json (id, ts, kind, app, title, site, text, file,
/// durationS). The copy is not encrypted: the UI warns before it asks for the path. Written to a .partial file
/// first, then moved over the target.
/// </summary>
/// <summary>A user purge or panic reached the rows of a running export: the partial file is deleted (B12).</summary>
public sealed class ExportPurgedException() : IOException("A purge ran during the export; nothing was written.");

public static class CaptureExporter
{
    private const int Page = 2000;

    /// <summary>Rows in the window, newest first, filtered by app (process name, with or without .exe, any case).</summary>
    public static IEnumerable<CaptureMeta> Rows(IStore store, ExportRequest r)
    {
        string? app = NormalizeApp(r.App);
        var seen = new HashSet<string>(StringComparer.Ordinal);
        var to = r.To;
        while (true)
        {
            var page = store.Timeline(r.From, to, Page);
            int fresh = 0;
            foreach (var m in page)
            {
                if (!seen.Add(m.Id)) continue;
                fresh++;
                if (app is null || NormalizeApp(m.App) == app) yield return m;
            }
            if (page.Count < Page || fresh == 0) yield break;
            // Timeline is newest first and excludes "to": step to just after the oldest row so equal timestamps are not lost.
            to = page[^1].Ts.AddMilliseconds(1);
        }
    }

    private static string? NormalizeApp(string? app)
    {
        if (string.IsNullOrWhiteSpace(app)) return null;
        var a = app.Trim();
        if (a.EndsWith(".exe", StringComparison.OrdinalIgnoreCase)) a = a[..^4];
        return a.ToLowerInvariant();
    }

    public static ExportResult Count(IStore store, ExportRequest r)
    {
        int n = 0, images = 0;
        foreach (var m in Rows(store, r))
        {
            n++;
            if (m.HasThumb) images++;
        }
        return new ExportResult(n, images, 0);
    }

    private static string Extension(string mime) => mime switch
    {
        "image/jpeg" => "jpg",
        "image/avif" => "avif",
        "image/jxl" => "jxl",
        "image/png" => "png",
        _ => "webp",
    };

    /// <summary>transcode(keyframe, "jpeg") converts when r.Jpeg is set (the agent's IThumbnailer.Transcode).</summary>
    public static ExportResult Write(IStore store, ExportRequest r, Func<ThumbData, string, ThumbData> transcode, CancellationToken ct)
    {
        var dir = Path.GetDirectoryName(r.Path);
        if (string.IsNullOrEmpty(dir) || !Directory.Exists(dir)) throw new DirectoryNotFoundException("Export folder not found.");
        var partial = r.Path + ".partial";
        var entries = new List<ExportEntry>();
        int images = 0;
        // A user purge (panic flush, purge last/range/app/site/id, purge all) that deletes a row already written, or whose
        // window overlaps the export's, aborts it: the rows written before the purge would stay in the plaintext zip (B12).
        var written = new HashSet<string>(StringComparer.Ordinal);
        bool purged = false;
        void OnPurged(PurgeNotice n)
        {
            if (!n.ByUser) return;
            bool hit = n.Reason == "all" || n.From is { } f && n.To is { } t && f < r.To && r.From <= t;
            if (!hit) lock (written) hit = n.Ids.Any(written.Contains);
            if (hit) Volatile.Write(ref purged, true);
        }
        var notices = store as EncryptedStore;
        if (notices is not null) notices.Purged += OnPurged;
        // Recorded while it exists, so a partial left by a killed agent is deleted at the next start (C3).
        var home = notices?.Home;
        bool created = false;
        try
        {
            ExportJournal.Add(home, partial);
            FileStream fs;
            try { fs = new FileStream(partial, FileMode.Create, FileAccess.Write, FileShare.None); }
            catch { ExportJournal.Forget(home, partial); throw; }
            created = true;
            using (fs)
            using (var zip = new ZipArchive(fs, ZipArchiveMode.Create))
            {
                foreach (var m in Rows(store, r).Reverse())
                {
                    ct.ThrowIfCancellationRequested();
                    if (Volatile.Read(ref purged)) throw new ExportPurgedException();
                    lock (written) written.Add(m.Id);
                    var detail = store.Get(m.Id);
                    if (detail is null) continue; // deleted meanwhile
                    string? file = null;
                    if (m.HasThumb && store.GetKeyframe(m.Id) is { } kf)
                    {
                        var img = r.Jpeg ? transcode(kf, "jpeg") : kf;
                        try
                        {
                            file = "images/" + m.Ts.ToLocalTime().ToString("yyyyMMdd-HHmmss-fff", CultureInfo.InvariantCulture) + "_" + m.Id + "." + Extension(img.Mime);
                            var e = zip.CreateEntry(file, CompressionLevel.NoCompression);
                            e.LastWriteTime = m.Ts;
                            using var es = e.Open();
                            es.Write(img.Bytes);
                            images++;
                        }
                        finally
                        {
                            Array.Clear(kf.Bytes);
                            if (!ReferenceEquals(img, kf)) Array.Clear(img.Bytes);
                        }
                    }
                    entries.Add(new ExportEntry(m.Id, m.Ts, m.Kind, m.App, detail.Title, detail.Site, detail.Text, file, detail.DurationS));
                }
                var json = zip.CreateEntry("captures.json", CompressionLevel.Optimal);
                using (var js = json.Open())
                    JsonSerializer.Serialize(js, entries, CoreJson.Default.ListExportEntry);
            }
            if (Volatile.Read(ref purged)) throw new ExportPurgedException();
            File.Move(partial, r.Path, overwrite: true);
            ExportJournal.Remove(home, partial);
            // A purge that landed during the move: the finished zip goes too.
            if (Volatile.Read(ref purged))
            {
                try { File.Delete(r.Path); } catch (Exception ex) { Log.Warn("export delete after purge", ex); }
                throw new ExportPurgedException();
            }
        }
        catch (Exception ex)
        {
            if (created)   // only ours: a file of that name that was there before stays
            {
                try { File.Delete(partial); } catch { }
                ExportJournal.Remove(home, partial);
            }
            // Purge all disposes the store under the export: its next read fails (also when the export subscribed after
            // the store's notice), but the user is owed the purge message (BC-S2).
            if ((Volatile.Read(ref purged) || ex is ObjectDisposedException) && ex is not ExportPurgedException) throw new ExportPurgedException();
            throw;
        }
        finally { if (notices is not null) notices.Purged -= OnPurged; }
        long bytes = new FileInfo(r.Path).Length;
        Log.Info($"export: {entries.Count} captures, {images} images, {bytes / 1024} KiB");
        return new ExportResult(entries.Count, images, bytes);
    }
}
