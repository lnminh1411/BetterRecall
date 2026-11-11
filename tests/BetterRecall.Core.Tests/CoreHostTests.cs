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
}
