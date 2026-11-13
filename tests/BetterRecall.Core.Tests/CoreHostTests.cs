// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using BetterRecall.Contracts;

namespace BetterRecall.Core.Tests;

public class CoreHostTests
{
    [Fact]
    public async Task Open_store_then_purge_all_starts_empty_with_a_new_key()
    {
        using var h = new TempHome("dpapi");
        var host = (CoreHostImpl)CoreHost.Create(new CoreHostOptions(h.Path, h.Path));
        await using (host)
        {
            Assert.Null(host.Store);
            await host.OpenStoreAsync(CancellationToken.None);
            Assert.NotNull(host.Store);
            Assert.True(host.Keys.KeyLoaded);
            Assert.Equal("dpapi", host.Keys.Protection);
            var id = host.Store!.AddAudioCapture(Caps.Audio(DateTimeOffset.UtcNow, "hello world"));
            var keyBefore = File.ReadAllBytes(DataPaths.KeyFile(h.Path));

            // Blob cipher works with the vault key.
            var blob = Path.Combine(DataPaths.ReplaysDir(h.Path), "clip.mp4.enc");
            using (var w = host.Blobs.OpenWrite(blob)) w.Write([1, 2, 3]);
            using (var r = host.Blobs.OpenRead(blob)) Assert.Equal(1, r.ReadByte());

            int purged = await host.PurgeAllAsync(CancellationToken.None);
            Assert.Equal(1, purged);
            Assert.False(File.Exists(blob));
            Assert.NotNull(host.Store);
            Assert.Null(host.Store!.Get(id));
            Assert.NotEqual(keyBefore, File.ReadAllBytes(DataPaths.KeyFile(h.Path)));
        }
        // Reopen with the new key.
        var host2 = CoreHost.Create(new CoreHostOptions(h.Path, h.Path));
        await using (host2)
        {
            await host2.OpenStoreAsync(CancellationToken.None);
            Assert.Equal(0, host2.Store!.Stats().Rows);
        }
    }

    [Fact]
    public async Task Purge_all_when_the_store_never_opened_deletes_its_files_and_starts_fresh()
    {
        // Data key lost (its TPM key deleted): store.db and keyframes stay unreadable until purge all removes them.
        using var h = new TempHome("dpapi");
        var db = DataPaths.StoreDb(h.Path);
        Directory.CreateDirectory(Path.GetDirectoryName(db)!);
        File.WriteAllBytes(db, [1, 2, 3]);
        File.WriteAllBytes(db + "-wal", [4]);
        var kf = DataPaths.KeyframesDir(h.Path);
        Directory.CreateDirectory(kf);
        File.WriteAllBytes(Path.Combine(kf, "2026-11.db"), [5]);
        var host = (CoreHostImpl)CoreHost.Create(new CoreHostOptions(h.Path, h.Path));
        await using (host)
        {
            Assert.Null(host.Store);
            Assert.Equal(0, await host.PurgeAllAsync(CancellationToken.None));
            Assert.False(File.Exists(Path.Combine(kf, "2026-11.db")));
            Assert.NotNull(host.Store);
            Assert.NotEqual(3, new FileInfo(db).Length);   // the new store.db, not the old bytes
            Assert.Equal(0, host.Store!.Stats().Rows);
        }
    }
}
