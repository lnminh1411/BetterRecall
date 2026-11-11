// SPDX-License-Identifier: MIT
// Copyright (c) BetterRecall contributors
using System.Text;
using BetterRecall.Contracts;
using BetterRecall.UI.Logic;
using BetterRecall.UI.Logic.Mock;

namespace BetterRecall.UI.Tests;

public class AgentClientTests
{
    [Fact]
    public async Task NoPipe_IsNotRunning()
    {
        await using var c = new AgentClient("BetterRecall.uitest.absent." + Guid.NewGuid().ToString("N"), MockHarness.OwnProcessTrust(), TimeSpan.FromMilliseconds(200));
        var r = await c.SendAsync("ping");
        Assert.False(r.Ok);
        Assert.Equal(AgentErrorKind.NotRunning, r.Kind);
    }

    [Fact]
    public async Task WrongServerImage_IsNotTrusted_AndNothingIsSent()
    {
        await using var server = new FakeLineServer(_ => Envelope.Ok("{\"pong\":true}"));
        var trust = new ServerTrustVerifier(@"C:\Program Files\BetterRecall\BetterRecall.Agent.exe");
        await using var c = new AgentClient(server.Name, trust);
        var r = await c.SendAsync("ping");
        Assert.Equal(AgentErrorKind.NotTrusted, r.Kind);
        await Task.Delay(50);
        lock (server.Received) Assert.Empty(server.Received);
    }

    [Fact]
    public async Task OtherSession_IsNotTrusted()
    {
        await using var server = new FakeLineServer(_ => Envelope.Ok("null"));
        var trust = new ServerTrustVerifier(Environment.ProcessPath!, sessionOf: _ => 99, ownSession: 1);
        await using var c = new AgentClient(server.Name, trust);
        Assert.Equal(AgentErrorKind.NotTrusted, (await c.SendAsync("ping")).Kind);
    }

    [Fact]
    public async Task UnknownServerPid_IsNotTrusted()
    {
        await using var server = new FakeLineServer(_ => Envelope.Ok("null"));
        var trust = new ServerTrustVerifier(Environment.ProcessPath!, serverPid: _ => null);
        await using var c = new AgentClient(server.Name, trust);
        Assert.Equal(AgentErrorKind.NotTrusted, (await c.SendAsync("ping")).Kind);
    }

    [Fact]
    public async Task ImageLookupFails_IsNotTrusted()
    {
        await using var server = new FakeLineServer(_ => Envelope.Ok("null"));
        var trust = new ServerTrustVerifier(Environment.ProcessPath!, imageOf: _ => null);
        await using var c = new AgentClient(server.Name, trust);
        Assert.Equal(AgentErrorKind.NotTrusted, (await c.SendAsync("ping")).Kind);
    }

    [Fact]
    public async Task TrustedServer_Works_AndReportsPid()
    {
        await using var server = new FakeLineServer(_ => Envelope.Ok("{\"pong\":true}"));
        await using var c = new AgentClient(server.Name, MockHarness.OwnProcessTrust());
        var r = await c.SendAsync("ping");
        Assert.True(r.Ok);
        Assert.True(r.Data.GetProperty("pong").GetBoolean());
        Assert.Equal(Environment.ProcessId, c.ServerPid);
    }

    [Fact]
    public void InstalledAgentPath_IsParentOfUiFolder()
    {
        var p = ServerTrustVerifier.InstalledAgentPath(@"C:\Program Files\BetterRecall\ui\");
        Assert.Equal(@"C:\Program Files\BetterRecall\BetterRecall.Agent.exe", p);
    }

    [Fact]
    public async Task StoreLocked_IsClassified()
    {
        await using var server = new FakeLineServer(_ => Envelope.Error("Store is locked."));
        await using var c = new AgentClient(server.Name, MockHarness.OwnProcessTrust());
        AgentErrorKind? seen = null;
        c.ResponseClassified += k => seen = k;
        var r = await new AgentApi(c).SearchAsync(new SearchQuery("x"));
        Assert.True(r.IsLocked);
        Assert.Equal(AgentErrorKind.Locked, seen);
    }

    [Fact]
    public async Task ServerClosesWithoutAnswer_IsNotRunning_ThenReconnects()
    {
        int n = 0;
        await using var server = new FakeLineServer(_ => Interlocked.Increment(ref n) == 1 ? null : Envelope.Ok("null"));
        await using var c = new AgentClient(server.Name, MockHarness.OwnProcessTrust());
        Assert.Equal(AgentErrorKind.NotRunning, (await c.SendAsync("ping")).Kind);
        Assert.True((await c.SendAsync("ping")).Ok);
    }

    [Fact]
    public async Task GarbageResponse_IsProtocolError()
    {
        await using var server = new FakeLineServer(_ => "not json");
        await using var c = new AgentClient(server.Name, MockHarness.OwnProcessTrust());
        Assert.Equal(AgentErrorKind.Protocol, (await c.SendAsync("ping")).Kind);
    }

    [Fact]
    public async Task WrongShape_IsProtocolError()
    {
        await using var server = new FakeLineServer(_ => Envelope.Ok("{\"nope\":1}"));
        await using var c = new AgentClient(server.Name, MockHarness.OwnProcessTrust());
        var r = await new AgentApi(c).TimelineAsync(DateTimeOffset.UtcNow.AddHours(-1), DateTimeOffset.UtcNow);
        Assert.Equal(AgentErrorKind.Protocol, r.Kind);
    }

    [Fact]
    public async Task OversizedRequest_IsRejectedLocally()
    {
        await using var server = new FakeLineServer(_ => Envelope.Ok("null"));
        await using var c = new AgentClient(server.Name, MockHarness.OwnProcessTrust());
        var r = await c.SendAsync("search " + new string('x', 5000));
        Assert.False(r.Ok);
        Assert.Equal(LineProtocol.TooLongError, r.Error);
        lock (server.Received) Assert.Empty(server.Received);
    }

    [Fact]
    public async Task LineReader_HandlesLargeLines_Crlf_Bom_AndSeveralLinesPerRead()
    {
        var big = new string('a', 200_000);
        var payload = Encoding.UTF8.GetBytes("﻿first\r\n" + big + "\nthird");
        var reader = new LineReader(new MemoryStream(payload));
        Assert.Equal("first", await reader.ReadLineAsync(1 << 20, default));
        Assert.Equal(big, await reader.ReadLineAsync(1 << 20, default));
        Assert.Equal("third", await reader.ReadLineAsync(1 << 20, default));
        Assert.Null(await reader.ReadLineAsync(1 << 20, default));
    }

    [Fact]
    public async Task LineReader_EnforcesLimit()
    {
        var reader = new LineReader(new MemoryStream(Encoding.UTF8.GetBytes(new string('a', 100_000) + "\n")));
        await Assert.ThrowsAsync<LineTooLongException>(() => reader.ReadLineAsync(70_000, default));
    }

    [Fact]
    public void Thumb_ContractsShapeRoundTrips()
    {
        var json = System.Text.Json.JsonSerializer.Serialize(new ThumbData("f_1", "image/webp", 320, 180, [9, 8]), ContractsJson.Default.ThumbData);
        Assert.Contains("\"base64\"", json);
        Assert.Equal([9, 8], Responses.Thumb(System.Text.Json.JsonDocument.Parse(json).RootElement)!.Bytes);
    }

    [Theory]
    [InlineData("Wrong passphrase.", "Err_WrongPassphrase")]
    [InlineData("Unlock required.", "Err_UnlockRequired")]
    public void NewErrorStrings_MapToResources(string error, string key) => Assert.Equal(key, SearchViewModel.ServerError(error).Key);

    [Fact]
    public void Enums_AreLowercaseOnTheWire()
    {
        var json = System.Text.Json.JsonSerializer.Serialize(new SearchHit("a", DateTimeOffset.UnixEpoch, CaptureKind.Audio, "x", "", 0.5f, "", false, 3), ContractsJson.Default.SearchHit);
        Assert.Contains("\"kind\":\"audio\"", json);
    }

    [Fact]
    public void Thumb_AcceptsBase64FieldToo()
    {
        var json = System.Text.Json.JsonDocument.Parse("{\"id\":\"f_1\",\"mime\":\"image/webp\",\"width\":320,\"height\":180,\"base64\":\"AQID\"}").RootElement;
        var t = Responses.Thumb(json)!;
        Assert.Equal([1, 2, 3], t.Bytes);
        Assert.Equal(320, t.Width);
    }

    [Fact]
    public void SearchLine_IsLowercaseJson_WithFilters()
    {
        var from = new DateTimeOffset(2026, 11, 1, 0, 0, 0, TimeSpan.Zero);
        var line = AgentApi.BuildSearchLine(new SearchQuery(" invoice ", 20, from, null, "chrome.exe", CaptureKind.Audio));
        Assert.Equal("search {\"q\":\"invoice\",\"limit\":20,\"from\":\"2026-11-01T00:00:00Z\",\"app\":\"chrome.exe\",\"kind\":[\"audio\"]}", line);
    }

    [Theory]
    [InlineData("Invalid capture.intervalMs.", "capture.intervalMs")]
    [InlineData("Invalid JSON.", null)]
    [InlineData("Invalid or expired token.", null)]
    [InlineData("Store is locked.", null)]
    public void InvalidKey_Parses(string error, string? key) => Assert.Equal(key, AgentErrors.InvalidKey(error));
}
