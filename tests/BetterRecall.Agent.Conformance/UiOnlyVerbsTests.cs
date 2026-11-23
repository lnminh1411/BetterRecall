// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Text.Json;
using BetterRecall.Agent.Conformance.Harness;
using BetterRecall.Contracts;

namespace BetterRecall.Agent.Conformance;

[Collection("AgentConformance")]
public sealed class UiOnlyVerbsTests(InstallFixture install)
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
    public async Task Ui_only_verbs_answer_on_ui_pipe_and_are_refused_on_public_pipe()
    {
        await using var h = new AgentHarness(install, s => s.Ipc.Enabled = true);
        await h.StartAndConnectUiAsync();
        await h.SetupPassphraseAndWaitStoreAsync();

        await using var pub = await h.WaitForPublicConnectAsync();

        // was: ModelsTests.Models_list_returns_all_eight_catalog_ids_on_ui_pipe_and_is_refused_on_public_pipe
        // UI pipe: models list returns all eight catalog ids even on a scratch home with no models installed.
        var (modelsOk, modelsData, modelsErr) = await h.Ui.RequestAsync("ui", "models list");
        Assert.True(modelsOk, modelsErr);
        var list = modelsData.Deserialize(ContractsJson.Default.ModelsList);
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
        var (pubModelsOk, _, pubModelsErr) = Envelope.Parse((await pub.RequestAsync("models list"))!);
        Assert.False(pubModelsOk);
        Assert.Equal("Unknown command 'models'.", pubModelsErr);

        // was: UpdateTests.Update_status_returns_default_state_on_ui_pipe_and_is_refused_on_public_pipe
        // UI pipe: update status returns auto=false and the running app version (no network).
        var (vOk, vData, vErr) = await h.Ui.RequestAsync("ui", "version");
        Assert.True(vOk, vErr);
        var version = vData.Deserialize(ContractsJson.Default.VersionReply);
        Assert.NotNull(version);

        var (updOk, updData, updErr) = await h.Ui.RequestAsync("ui", "update status");
        Assert.True(updOk, updErr);
        var status = updData.Deserialize(ContractsJson.Default.UpdateStatus);
        Assert.NotNull(status);
        Assert.False(status!.Auto);
        Assert.Equal(version!.App, status.CurrentVersion);
        Assert.Equal("idle", status.State);
        Assert.False(status.Available);

        // Public pipe: update is UI-only and must be refused.
        var (pubUpdOk, _, pubUpdErr) = Envelope.Parse((await pub.RequestAsync("update status"))!);
        Assert.False(pubUpdOk);
        Assert.Equal("Unknown command 'update'.", pubUpdErr);
    }
}
