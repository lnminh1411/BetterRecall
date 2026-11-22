// SPDX-License-Identifier: MIT
// Copyright (c) BetterRecall contributors
using System.Globalization;
using System.Text.Json;
using BetterRecall.Contracts;
using BetterRecall.UI.Logic;

namespace BetterRecall.UI.Tests;

public class ModelsTests
{
    [Fact]
    public void Responses_Parses_ModelsList_And_ModelsProgress()
    {
        const string listJson =
            """{"modelsDir":"C:\\BetterRecall\\models","repo":"lnminh1411/BetterRecall-models","revision":"893eb2c63a11443e9f5061745c69ecbd5e1bd16a","components":[{"id":"embeddinggemma-2","title":"EmbeddingGemma 2","description":"Search","sets":["full","minimal"],"bytes":2782236945,"state":"complete"},{"id":"silero-vad","title":"Silero VAD","description":"Speech detection for transcription","sets":["full"],"bytes":2329089,"state":"missing"}],"fullBytes":6555959572,"minimalBytes":3476218217,"freeBytes":53687091200,"downloading":false}""";
        using var listDoc = JsonDocument.Parse(listJson);
        var list = Responses.ModelsList(listDoc.RootElement);
        Assert.NotNull(list);
        Assert.Equal(@"C:\BetterRecall\models", list!.ModelsDir);
        Assert.Equal("lnminh1411/BetterRecall-models", list.Repo);
        Assert.Equal("893eb2c63a11443e9f5061745c69ecbd5e1bd16a", list.Revision);
        Assert.Equal(2, list.Components.Length);
        Assert.Equal("embeddinggemma-2", list.Components[0].Id);
        Assert.Equal("complete", list.Components[0].State);
        Assert.Equal(["full", "minimal"], list.Components[0].Sets);
        Assert.Equal(6_555_959_572L, list.FullBytes);
        Assert.Equal(3_476_218_217L, list.MinimalBytes);
        Assert.Equal(53_687_091_200L, list.FreeBytes);
        Assert.False(list.Downloading);

        const string progJson =
            """{"phase":"downloading","doneBytes":524288000,"totalBytes":2782236945,"file":"embeddinggemma-2/vision.onnx"}""";
        using var progDoc = JsonDocument.Parse(progJson);
        var prog = Responses.ModelsProgress(progDoc.RootElement);
        Assert.NotNull(prog);
        Assert.Equal("downloading", prog!.Phase);
        Assert.Equal(524_288_000L, prog.DoneBytes);
        Assert.Equal(2_782_236_945L, prog.TotalBytes);
        Assert.Equal("embeddinggemma-2/vision.onnx", prog.File);
        Assert.Null(prog.Error);
    }

    [Fact]
    public void FormatSize_Uses_GB_And_MB_With_One_Decimal()
    {
        var inv = CultureInfo.InvariantCulture;
        Assert.Equal("6.6 GB", ModelsFormat.FormatSize(6_555_959_572L, inv));
        Assert.Equal("3.5 GB", ModelsFormat.FormatSize(3_476_218_217L, inv));
        Assert.Equal("1.0 GB", ModelsFormat.FormatSize(1_000_000_000L, inv));
        Assert.Equal("430.6 MB", ModelsFormat.FormatSize(430_604_834L, inv));
        Assert.Equal("2.3 MB", ModelsFormat.FormatSize(2_329_089L, inv));
        Assert.Equal("0.0 MB", ModelsFormat.FormatSize(0, inv));
    }

    [Fact]
    public void FreeSpaceCheck_Requires_Size_Plus_Five_Percent()
    {
        long size = 1_000_000_000L; // 1.0 GB -> needs 1,050,000,000 bytes
        Assert.Equal(1_050_000_000L, ModelsFormat.RequiredFreeBytes(size));
        Assert.True(ModelsFormat.HasEnoughSpace(size, 1_050_000_000L));
        Assert.False(ModelsFormat.HasEnoughSpace(size, 1_049_999_999L));
        Assert.Equal(1L, ModelsFormat.MissingSpaceBytes(size, 1_049_999_999L));
        Assert.Equal(0L, ModelsFormat.MissingSpaceBytes(size, 2_000_000_000L));
    }

    [Fact]
    public void Speed_And_Progress_Text_Format_Correctly()
    {
        var inv = CultureInfo.InvariantCulture;
        Assert.Equal("12.5 MB/s", ModelsFormat.FormatSpeed(12_500_000.0, inv));
        Assert.Equal("0.0 MB/s", ModelsFormat.FormatSpeed(-5.0, inv));
        Assert.Equal("1.2 of 3.5 GB, 12.5 MB/s", ModelsFormat.FormatProgress(1_200_000_000L, 3_500_000_000L, 12_500_000.0, inv));
        Assert.Equal("50.0 of 100.0 MB, 5.2 MB/s", ModelsFormat.FormatProgress(50_000_000L, 100_000_000L, 5_200_000.0, inv));
        Assert.Equal("893eb2c", ModelsFormat.ShortRevision("893eb2c63a11443e9f5061745c69ecbd5e1bd16a"));
    }

    [Fact]
    public async Task AgentApi_ModelsList_And_DownloadStream_Parse_Over_Pipe()
    {
        var list = new ModelsList(
            @"K:\scratch\models",
            "lnminh1411/BetterRecall-models",
            "893eb2c63a11443e9f5061745c69ecbd5e1bd16a",
            [new ModelComponent("silero-vad", "Silero VAD", "Speech detection for transcription", ["full"], 2_329_089, "missing")],
            6_555_959_572L,
            3_476_218_217L,
            50_000_000_000L,
            false);

        await using var listServer = new FakeLineServer(line =>
            line == "models list"
                ? Envelope.Ok(list, ContractsJson.Default.ModelsList)
                : Envelope.Error("unexpected"));
        await using var listClient = new AgentClient(listServer.Name, MockHarness.OwnProcessTrust());
        var api = new AgentApi(listClient);

        var r = await api.ModelsListAsync();
        Assert.True(r.Ok, r.Error);
        Assert.Equal(@"K:\scratch\models", r.Value!.ModelsDir);
        Assert.Single(r.Value.Components);
        Assert.Equal("silero-vad", r.Value.Components[0].Id);
        Assert.Equal(2_329_089L, ModelsFormat.DownloadSizeFor(r.Value, "silero-vad"));
        Assert.Equal(3_476_218_217L, ModelsFormat.DownloadSizeFor(r.Value, "minimal"));
        Assert.Equal(6_555_959_572L, ModelsFormat.DownloadSizeFor(r.Value, "full"));
    }
}
