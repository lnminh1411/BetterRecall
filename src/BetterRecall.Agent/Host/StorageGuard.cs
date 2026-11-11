// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using BetterRecall.Contracts;

namespace BetterRecall.Agent.Host;

/// <summary>Result of one storage check. Why: "disk" (storage.minFreeGb) or "size" (storage.maxGb), empty when fine.</summary>
internal sealed record StorageDecision(bool Pause, bool Notify, int Deleted, string Why);

/// <summary>
/// Storage safeguard (v0.2 batch 2). Free space counts the disk's free bytes plus the store's reusable free pages;
/// size counts the store's live bytes. Below storage.minFreeGb or above storage.maxGb capture pauses (reason
/// "storage"), unless storage.overwrite is on: then the oldest captures go in progressive order (IStore.DeleteForSpace)
/// until there is a margin again. A pause ends once the margin is back, so it does not flap.
/// </summary>
internal sealed class StorageGuard(Func<IStore?> store, Func<Settings> settings, Func<long?> diskFree)
{
    public const long Gb = 1024L * 1024 * 1024;
    public const int Batch = 500;
    public const int MaxBatchesPerCheck = 200;

    public bool Paused { get; private set; }

    /// <summary>Margin above the limits: 1 GB, at most a twentieth of storage.maxGb.</summary>
    public static long Margin(StorageSettings s) => Math.Min(Gb, s.MaxGb * Gb / 20);

    public static bool LowDisk(StorageSettings s, long? disk, StorageUse use, long margin) =>
        s.MinFreeGb > 0 && disk is long d && d + use.FreeBytes < s.MinFreeGb * Gb + margin;

    public static bool OverSize(StorageSettings s, StorageUse use, long margin) =>
        use.LiveBytes > s.MaxGb * Gb - margin;

    public StorageDecision Check()
    {
        var st = store();
        if (st is null) return new StorageDecision(Paused, false, 0, "");
        var s = settings().Storage;
        long margin = Margin(s);
        var use = st.GetStorageUse();
        long? disk = diskFree();
        int deleted = 0;
        if (s.Overwrite && (LowDisk(s, disk, use, 0) || OverSize(s, use, 0)))
        {
            for (int i = 0; i < MaxBatchesPerCheck && (LowDisk(s, disk, use, margin) || OverSize(s, use, margin)); i++)
            {
                int n = st.DeleteForSpace(Batch);
                if (n == 0) break;
                deleted += n;
                use = st.GetStorageUse();
                disk = diskFree();
            }
        }
        // While paused, resuming needs the margin back.
        long resume = Paused ? margin : 0;
        bool low = LowDisk(s, disk, use, resume);
        bool over = OverSize(s, use, resume);
        bool pause = low || over;
        bool notify = pause && !Paused;
        Paused = pause;
        return new StorageDecision(pause, notify, deleted, low ? "disk" : over ? "size" : "");
    }

    /// <summary>Free bytes of the volume holding path, null when unknown (network paths).</summary>
    public static long? FreeBytesOf(string path)
    {
        try
        {
            var root = Path.GetPathRoot(Path.GetFullPath(path));
            if (string.IsNullOrEmpty(root) || root.StartsWith(@"\\", StringComparison.Ordinal)) return null;
            return new DriveInfo(root).AvailableFreeSpace;
        }
        catch (Exception) { return null; }
    }
}
