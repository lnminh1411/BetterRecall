// SPDX-License-Identifier: MIT
// Copyright (c) BetterRecall contributors
using System.Globalization;
using System.Text.Json;
using BetterRecall.Contracts;
using BetterRecall.UI.Logic;

namespace BetterRecall.UI.Tests;

public class UpdateTests
{
    [Fact]
    public void Responses_Parses_UpdateStatus_And_UpdateProgress()
    {
        const string statusJson =
            """{"currentVersion":"0.3.0-dev","latestVersion":"0.3.0","available":true,"canInstall":true,"notesUrl":"https://github.com/lnminh1411/BetterRecall/releases/tag/v0.3.0","publishedAt":"2026-11-22T10:00:00Z","installerBytes":125829120,"state":"idle","lastCheck":"2026-11-22T10:05:00Z","auto":false}""";
        using var statusDoc = JsonDocument.Parse(statusJson);
        var st = Responses.UpdateStatus(statusDoc.RootElement);
        Assert.NotNull(st);
        Assert.Equal("0.3.0-dev", st!.CurrentVersion);
        Assert.Equal("0.3.0", st.LatestVersion);
        Assert.True(st.Available);
        Assert.True(st.CanInstall);
        Assert.Null(st.Reason);
        Assert.Equal("https://github.com/lnminh1411/BetterRecall/releases/tag/v0.3.0", st.NotesUrl);
        Assert.Equal(125_829_120L, st.InstallerBytes);
        Assert.Equal("idle", st.State);
        Assert.False(st.Auto);

        const string progJson =
            """{"phase":"downloading","doneBytes":52428800,"totalBytes":125829120}""";
        using var progDoc = JsonDocument.Parse(progJson);
        var prog = Responses.UpdateProgress(progDoc.RootElement);
        Assert.NotNull(prog);
        Assert.Equal("downloading", prog!.Phase);
        Assert.Equal(52_428_800L, prog.DoneBytes);
        Assert.Equal(125_829_120L, prog.TotalBytes);
        Assert.Null(prog.Error);
    }

    [Fact]
    public async Task UpdateDownloadStreamAsync_parses_progress_and_handles_bad_response()
    {
        int call = 0;
        await using var server = new FakeLineServer(line =>
        {
            if (line != "update download") return [Envelope.Error("unexpected")];
            return Interlocked.Increment(ref call) == 1
                ?
                [
                    Envelope.Ok("""{"phase":"downloading","doneBytes":52428800,"totalBytes":125829120}"""),
                    Envelope.Ok("""{"phase":"done","doneBytes":125829120,"totalBytes":125829120}"""),
                ]
                : [Envelope.Ok("123")];
        });

        var progress = new List<UpdateProgress>();
        await using (var run = new AgentClient(server.Name, MockHarness.OwnProcessTrust()))
        {
            var r = await AgentApi.UpdateDownloadStreamAsync(run, progress.Add);
            Assert.True(r.Ok, r.Error);
        }
        Assert.Equal(["downloading", "done"], progress.Select(p => p.Phase));
        Assert.Equal(52_428_800L, progress[0].DoneBytes);
        Assert.Equal(125_829_120L, progress[1].DoneBytes);

        var bad = new List<UpdateProgress>();
        await using (var run = new AgentClient(server.Name, MockHarness.OwnProcessTrust()))
        {
            var r = await AgentApi.UpdateDownloadStreamAsync(run, bad.Add);
            Assert.True(r.Ok, r.Error);
        }
        var err = Assert.Single(bad);
        Assert.Equal("failed", err.Phase);
        Assert.Equal("Bad response.", err.Error);
    }

    [Fact]
    public void Classify_Selects_Expected_Status_Kinds()
    {
        Assert.Equal(UpdateStatusKind.NotChecked, UpdateLogic.Classify(null));

        var baseStatus = new UpdateStatus(
            "0.3.0-dev", null, false, true, null, null, null, null, "idle", null, null, false);
        Assert.Equal(UpdateStatusKind.NotChecked, UpdateLogic.Classify(baseStatus));

        var upToDate = baseStatus with { LastCheck = DateTimeOffset.UtcNow };
        Assert.Equal(UpdateStatusKind.UpToDate, UpdateLogic.Classify(upToDate));

        var available = upToDate with { LatestVersion = "0.3.0", Available = true };
        Assert.Equal(UpdateStatusKind.Available, UpdateLogic.Classify(available));

        var ready = available with { DownloadedVersion = "0.3.0", State = "ready" };
        Assert.Equal(UpdateStatusKind.Ready, UpdateLogic.Classify(ready));

        var applying = ready with { State = "applying" };
        Assert.Equal(UpdateStatusKind.Applying, UpdateLogic.Classify(applying));

        var err = upToDate with { State = "error", Error = "No internet connection." };
        Assert.Equal(UpdateStatusKind.Error, UpdateLogic.Classify(err));
    }

    [Fact]
    public void Size_And_Speed_Formatting()
    {
        var inv = CultureInfo.InvariantCulture;
        Assert.Equal("125.8 MB", UpdateLogic.FormatSize(125_829_120L, inv));
        Assert.Equal("1.5 GB", UpdateLogic.FormatSize(1_500_000_000L, inv));
        Assert.Equal("8.4 MB/s", UpdateLogic.FormatSpeed(8_400_000.0, inv));
        Assert.Equal("50.0 of 120.0 MB, 8.4 MB/s", UpdateLogic.FormatProgress(50_000_000L, 120_000_000L, 8_400_000.0, inv));
    }

    [Fact]
    public void NotesUrl_AllowCheck_Requires_Repo_Releases_Prefix()
    {
        Assert.True(UpdateLogic.IsAllowedNotesUrl("https://github.com/lnminh1411/BetterRecall/releases/tag/v0.3.0"));
        Assert.False(UpdateLogic.IsAllowedNotesUrl(null));
        Assert.False(UpdateLogic.IsAllowedNotesUrl(""));
        Assert.False(UpdateLogic.IsAllowedNotesUrl("http://github.com/lnminh1411/BetterRecall/releases/tag/v0.3.0"));
        Assert.False(UpdateLogic.IsAllowedNotesUrl("https://github.com/other/repo/releases/tag/v0.3.0"));
        Assert.False(UpdateLogic.IsAllowedNotesUrl("https://evil.example/https://github.com/lnminh1411/BetterRecall/releases/tag/v0.3.0"));
    }

    [Fact]
    public void DisabledState_When_CanInstall_Is_False()
    {
        var noKey = new UpdateStatus(
            "0.3.0-dev",
            "0.3.0",
            true,
            false,
            "This build cannot verify updates (it was built without the release key).",
            "https://github.com/lnminh1411/BetterRecall/releases/tag/v0.3.0",
            null,
            null,
            "idle",
            null,
            DateTimeOffset.UtcNow,
            false);

        Assert.False(UpdateLogic.CanDownload(noKey, downloading: false, applying: false));
        Assert.False(UpdateLogic.CanToggleAuto(noKey));

        var withKey = noKey with { CanInstall = true, Reason = null };
        Assert.True(UpdateLogic.CanDownload(withKey, downloading: false, applying: false));
        Assert.False(UpdateLogic.CanDownload(withKey, downloading: true, applying: false));
        Assert.False(UpdateLogic.CanDownload(withKey, downloading: false, applying: true));
        Assert.True(UpdateLogic.CanToggleAuto(withKey));
    }
}
