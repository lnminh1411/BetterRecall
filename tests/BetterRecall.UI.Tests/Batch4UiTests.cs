// SPDX-License-Identifier: MIT
// Copyright (c) BetterRecall contributors
using System.Text.Json;
using System.Text.Json.Nodes;
using BetterRecall.Contracts;
using BetterRecall.UI.Logic;
using BetterRecall.UI.Logic.Mock;
using Xunit;

namespace BetterRecall.UI.Tests;

public sealed class Batch4LogicTests
{
    private static SearchHit Hit(string id, CaptureKind kind, bool text, string? replay = null, int? offset = null) =>
        new(id, DateTimeOffset.UtcNow, kind, "game.exe", "t " + id, 0.5f, "", kind != CaptureKind.Audio, null, 0.5f, text, false, "", replay, offset);

    [Fact]
    public void Replay_frames_go_to_the_replay_section_whatever_matched()
    {
        var items = new[]
        {
            Hit("t1", CaptureKind.Screen, true), Hit("p1", CaptureKind.Replay, true, "r_001", 5000),
            Hit("p2", CaptureKind.Replay, false, "r_001", 10000), Hit("a1", CaptureKind.Audio, true),
        }.Select(h => CaptureItem.From(h, "q"));
        var groups = ResultGroups.Build(items);
        var replay = Assert.Single(groups, g => g.Kind == ResultGroupKind.Replay);
        Assert.Equal(["p1", "p2"], replay.Select(i => i.Id));
        Assert.True(replay[0].IsReplay);
        Assert.True(replay[0].HasImage);
        Assert.Equal("r_001", replay[0].ReplayId);
        Assert.Equal(5000, replay[0].ReplayOffsetMs);
        Assert.DoesNotContain(groups.Where(g => g.Kind != ResultGroupKind.Replay).SelectMany(g => g), i => i.IsReplay);
    }

    [Theory]
    [InlineData("r_001@83000", "r_001", 83000)]
    [InlineData("20261112T100000Z@0", "20261112T100000Z", 0)]
    public void ReplayLink_roundtrips(string arg, string id, int ms)
    {
        Assert.Equal((id, ms), ReplayLink.Parse(arg));
        Assert.Equal("replays:" + arg, ReplayLink.Page(id, ms));
    }

    [Theory]
    [InlineData(null)]
    [InlineData("r_001")]
    [InlineData("r_001@")]
    [InlineData("@5")]
    [InlineData("r_001@-5")]
    [InlineData("r 1@5")]
    [InlineData("..\\x@5")]
    public void ReplayLink_rejects(string? arg) => Assert.Null(ReplayLink.Parse(arg));

    [Fact]
    public void ReplayLink_clamps_negative_offset() => Assert.Equal("replays:r@0", ReplayLink.Page("r", -10));

    [Theory]
    [InlineData("auto", "hello", false, UnlockMode.Hello)]
    [InlineData("auto", "password", false, UnlockMode.Password)]
    [InlineData("passphrase", "hello", false, UnlockMode.Passphrase)]
    [InlineData("passphrase", "password", false, UnlockMode.Password)]
    [InlineData("passphrase", "password", true, UnlockMode.Passphrase)]   // the key itself still needs its passphrase
    [InlineData("tpm", "hello", true, UnlockMode.Hello)]
    public void Unlock_mode(string protection, string method, bool keyLocked, UnlockMode mode) =>
        Assert.Equal(mode, UnlockLogic.Mode(protection, method, keyLocked));

    [Fact]
    public void Retry_after_parses_the_agent_message()
    {
        Assert.Equal(30, UnlockLogic.RetryAfter("Too many attempts. Try again in 30 s."));
        Assert.Null(UnlockLogic.RetryAfter("Wrong password."));
        Assert.Null(UnlockLogic.RetryAfter(null));
        Assert.Equal("Err_TooManyTries", SearchViewModel.ServerError("Too many attempts. Try again in 900 s.").Key);
        Assert.Equal("Err_WrongPassword", SearchViewModel.ServerError(AgentErrors.WrongPassword).Key);
    }

    [Fact]
    public void Password_json_escapes_and_omits_nulls()
    {
        var j = JsonNode.Parse(AgentApi.PasswordJson("a\"b\\c", null))!.AsObject();
        Assert.Equal("a\"b\\c", j["current"]!.GetValue<string>());
        Assert.False(j.ContainsKey("new"));
        Assert.Equal("{\"new\":\"x y\"}", AgentApi.PasswordJson(null, "x y"));
        Assert.DoesNotContain('\n', AgentApi.PasswordJson("line\nbreak", "z"));
    }

    [Fact]
    public void Schema_has_new_keys_with_rules()
    {
        Assert.Equal(["hello", "password"], SettingsSchema.Find("security.unlockMethod")!.Choices!);
        Assert.Equal(["gemini", "openai-compatible", "claude"], SettingsSchema.Find("ai.provider")!.Choices!);
        Assert.Equal("ai.enabled", SettingsSchema.Find("ai.model")!.EnabledWhenKey);
        Assert.Equal(2, SettingsSchema.Find("replay.sampleSeconds")!.Min);
        Assert.Equal(60, SettingsSchema.Find("replay.sampleSeconds")!.Max);
        Assert.Null(SettingsSchema.Find("ai.apiKey"));
        Assert.Contains("ai", SettingsSchema.Groups);

        var root = JsonNode.Parse("{\"ai\":{\"enabled\":true,\"provider\":\"gemini\",\"endpoint\":\"https://x.example\",\"model\":\"bad model\"},\"replay\":{\"sampleSeconds\":5}}")!.AsObject();
        var errors = SettingsSchema.ValidateAll(root);
        Assert.Contains("ai.model", errors.Keys);
        Assert.DoesNotContain("ai.endpoint", errors.Keys);
    }

    [Fact]
    public void Mock_password_arg()
    {
        var a = AppArgs.Parse(["--mock-password"]);
        Assert.True(a.Mock);
        Assert.True(a.MockLocked);
        Assert.True(a.MockPassword);
    }
}

public sealed class Batch4MockTests
{
    [Fact]
    public async Task Password_unlock_rate_limits_after_five_wrong()
    {
        await using var h = new MockHarness(new MockOptions(StartLocked: true, PasswordUnlock: true));
        var s = await h.Api.SettingsGetAsync();
        Assert.Equal("password", SettingsSchema.Get(s.Value!, "security.unlockMethod")!.GetValue<string>());
        for (int i = 0; i < 4; i++) Assert.Equal(AgentErrors.WrongPassword, (await h.Api.UnlockAsync("wrong")).Error);
        var blocked = await h.Api.UnlockAsync("wrong");
        Assert.NotNull(UnlockLogic.RetryAfter(blocked.Error));
        Assert.True((await h.Api.PasswordStatusAsync()).Value!.RetryAfterS > 0);
    }

    [Fact]
    public async Task Password_unlock_opens()
    {
        await using var h = new MockHarness(new MockOptions(StartLocked: true, PasswordUnlock: true));
        Assert.True((await h.Api.UnlockAsync("open sesame")).Ok);
        Assert.True((await h.Api.SearchAsync(new SearchQuery("invoice"))).Ok);
    }

    [Fact]
    public async Task Password_change_and_method_guard()
    {
        await using var h = new MockHarness();
        Assert.Equal(AgentErrors.WrongPassword, (await h.Api.PasswordSetAsync("wrong", "new-password")).Error);
        Assert.Equal(AgentErrors.PasswordRule, (await h.Api.PasswordSetAsync("old", "short")).Error);
        Assert.True((await h.Api.PasswordSetAsync("old", "new-password")).Ok);
        Assert.True((await h.Api.PasswordClearAsync("new-password")).Ok);
        Assert.False((await h.Api.PasswordStatusAsync()).Value!.Set);
        var r = await h.Api.SettingsSetAsync(JsonNode.Parse("{\"security\":{\"unlockMethod\":\"password\"}}")!.AsObject());
        Assert.Equal(AgentErrors.NoPassword, r.Error);
    }

    [Fact]
    public async Task Ai_key_status_never_returns_the_key()
    {
        await using var h = new MockHarness();
        var st = await h.Api.AiSetKeyAsync("mock-key-123");
        Assert.True(st.Ok);
        Assert.True(st.Value!.KeySet);
        Assert.False((await h.Api.AiClearKeyAsync()).Value!.KeySet);
        var settings = await h.Api.SettingsGetAsync();
        Assert.DoesNotContain("mock-key-123", settings.Value!.ToJsonString());
    }

    [Fact]
    public async Task Describe_needs_preview_token()
    {
        await using var h = new MockHarness();
        var hits = await h.Api.SearchAsync(new SearchQuery("exe", Kind: CaptureKind.Screen));
        var id = hits.Value!.Results.First(x => x.HasThumb).Id;
        var p = await h.Api.DescribePreviewAsync(id);
        Assert.True(p.Ok, p.Error);
        Assert.NotEmpty(p.Value!.Bytes);
        Assert.False(string.IsNullOrEmpty(p.Value.Provider));
        Assert.Equal(AgentErrors.InvalidToken, (await h.Api.DescribeSendAsync("deadbeef")).Error);
        var d = await h.Api.DescribeSendAsync(p.Value.Token);
        Assert.True(d.Ok, d.Error);
        Assert.False(string.IsNullOrWhiteSpace(d.Value!.Text));
        Assert.Equal(AgentErrors.InvalidToken, (await h.Api.DescribeSendAsync(p.Value.Token)).Error);   // single use
    }

    [Fact]
    public async Task Replay_matches_carry_replay_and_offset_and_skip_timeline()
    {
        await using var h = new MockHarness();
        var r = await h.Api.SearchAsync(new SearchQuery("exe", Limit: 200, Kind: CaptureKind.Replay));
        Assert.True(r.Ok, r.Error);
        Assert.NotEmpty(r.Value!.Results);
        Assert.All(r.Value.Results, x =>
        {
            Assert.Equal(CaptureKind.Replay, x.Kind);
            Assert.NotNull(x.ReplayId);
            Assert.Equal(0, x.ReplayOffsetMs!.Value % 5000);
        });
        var replays = (await h.Api.ReplayListAsync()).Value!;
        Assert.Contains(replays, c => c.Id == r.Value.Results[0].ReplayId);
    }
}

public sealed class NetworkGuaranteeTests
{
    private static Dictionary<string, string> Strings()
    {
        var dir = new DirectoryInfo(AppContext.BaseDirectory);
        while (dir is not null && !File.Exists(Path.Combine(dir.FullName, "BetterRecall.slnx"))) dir = dir.Parent;
        Assert.NotNull(dir);
        var resw = System.Xml.Linq.XDocument.Load(Path.Combine(dir!.FullName, "src", "BetterRecall.UI", "Strings", "en-US", "Resources.resw"));
        return resw.Root!.Elements("data").ToDictionary(e => (string)e.Attribute("name")!, e => (string)e.Element("value")!);
    }

    [Fact]
    public void Turning_cloud_on_asks_first_turning_off_does_not()
    {
        Assert.True(NetworkGuarantee.NeedsConfirm("ai.enabled", on: true, savedOn: false));
        Assert.False(NetworkGuarantee.NeedsConfirm("ai.enabled", on: false, savedOn: false));
        Assert.False(NetworkGuarantee.NeedsConfirm("ai.enabled", on: false, savedOn: true));
        Assert.False(NetworkGuarantee.NeedsConfirm("ai.enabled", on: true, savedOn: true));   // already on: nothing new ends
        Assert.False(NetworkGuarantee.NeedsConfirm("replay.enabled", on: true, savedOn: false));
    }

    [Fact]
    public void Guarantee_state_follows_saved_ai_enabled()
    {
        Assert.False(NetworkGuarantee.IsOff(null));
        Assert.False(NetworkGuarantee.IsOff(JsonNode.Parse("{}")!.AsObject()));
        Assert.False(NetworkGuarantee.IsOff(JsonNode.Parse("{\"ai\":{\"enabled\":false}}")!.AsObject()));
        Assert.True(NetworkGuarantee.IsOff(JsonNode.Parse("{\"ai\":{\"enabled\":true}}")!.AsObject()));
        Assert.Equal("S_ai_provider_claude", NetworkGuarantee.ProviderLabelKey(JsonNode.Parse("{\"ai\":{\"provider\":\"claude\"}}")!.AsObject()));
        Assert.Equal("S_ai_provider_gemini", NetworkGuarantee.ProviderLabelKey(JsonNode.Parse("{\"ai\":{\"provider\":\"evil\"}}")!.AsObject()));
    }

    [Fact]
    public void Warning_texts_name_the_guarantee()
    {
        var s = Strings();
        var d = SettingsSchema.Find(NetworkGuarantee.SettingKey)!;
        Assert.Equal("W_ai_enabled", d.WarningKey);
        Assert.Contains("no-network guarantee", s[d.WarningKey!], StringComparison.OrdinalIgnoreCase);
        Assert.Contains("guarantee", s[NetworkGuarantee.ConfirmTitleKey], StringComparison.OrdinalIgnoreCase);
        var body = s[NetworkGuarantee.ConfirmBodyKey];
        Assert.StartsWith("Turning this on ends the no-network guarantee", body);
        Assert.Contains("{0}", body);   // the provider is named
        Assert.Contains("restore the guarantee", body);
        Assert.Contains("No-network guarantee off", s[NetworkGuarantee.OffLineKey]);
        Assert.True(s.ContainsKey(NetworkGuarantee.ConfirmButtonKey));
        foreach (var p in BetterRecall.Contracts.AiDefaults.Providers)
            Assert.True(s.ContainsKey(NetworkGuarantee.ProviderLabelKey(new JsonObject { ["ai"] = new JsonObject { ["provider"] = p } })), p);
    }
}
