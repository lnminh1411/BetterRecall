// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Text.Json;
using BetterRecall.Agent.Conformance.Harness;
using BetterRecall.Contracts;

namespace BetterRecall.Agent.Conformance;

[Collection("AgentConformance")]
public sealed class ModelsTests(InstallFixture install)
{
    private static readonly string[] ExpectedCatalogIds =
    [
        "embeddinggemma-2",
        "nsfw-classifier",
        "nsfw-detector",
        "ocr",
        "ner",
        "siglip2-base",
        "whisper-large-v3-turbo",
        "silero-vad",
    ];

    [Fact]
    public async Task Models_list_returns_all_eight_catalog_ids_on_ui_pipe_and_is_refused_on_public_pipe()
    {
        await using var h = new AgentHarness(install, s => s.Ipc.Enabled = true);
        await h.StartAndConnectUiAsync();
        await h.SetupPassphraseAndWaitStoreAsync();

        // UI pipe: models list returns all eight catalog ids even on a scratch home with no models installed.
        var (ok, data, err) = await h.Ui.RequestAsync("ui", "models list");
        Assert.True(ok, err);
        var list = data.Deserialize(ContractsJson.Default.ModelsList);
        Assert.NotNull(list);
        Assert.Equal(ExpectedCatalogIds.Length, list!.Components.Length);
        var ids = list.Components.Select(c => c.Id).ToHashSet(StringComparer.Ordinal);
        foreach (var id in ExpectedCatalogIds)
            Assert.Contains(id, ids);
        Assert.All(list.Components, c => Assert.Equal("missing", c.State));
        Assert.False(list.Downloading);
        Assert.True(list.FullBytes > list.MinimalBytes);
        Assert.True(list.MinimalBytes > 0);
        Assert.True(list.FreeBytes > 0);
        Assert.False(string.IsNullOrWhiteSpace(list.ModelsDir));

        // Public pipe: models is UI-only and must be refused.
        await using var pub = await h.WaitForPublicConnectAsync();
        var (pubOk, _, pubErr) = Envelope.Parse((await pub.RequestAsync("models list"))!);
        Assert.False(pubOk);
        Assert.Equal("Unknown command 'models'.", pubErr);
    }
}
