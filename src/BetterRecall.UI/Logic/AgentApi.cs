// SPDX-License-Identifier: MIT
// Copyright (c) BetterRecall contributors
using System.Globalization;
using System.Text;
using System.Text.Json;
using System.Text.Json.Nodes;
using BetterRecall.Contracts;

namespace BetterRecall.UI.Logic;


/// <summary>Filters the launcher sends with a search.</summary>
public sealed record SearchQuery(
    string Text,
    int? Limit = null,
    DateTimeOffset? From = null,
    DateTimeOffset? To = null,
    string? App = null,
    CaptureKind? Kind = null,
    SearchMode Mode = SearchMode.Hybrid,
    string? Site = null);

/// <summary>Parsers for every verb's data. Return null on a shape mismatch.</summary>
public static class Responses
{
    public static AgentStatus? Status(JsonElement d) => d.Deserialize(ContractsJson.Default.AgentStatus);
    public static SearchResponse? Search(JsonElement d) => d.Deserialize(ContractsJson.Default.SearchResponse);
    public static CaptureDetail? Detail(JsonElement d) => d.Deserialize(ContractsJson.Default.CaptureDetail);
    public static BenchResult? Bench(JsonElement d) => d.Deserialize(ContractsJson.Default.BenchResult);
    public static ReplayClip? Replay(JsonElement d) => d.Deserialize(ContractsJson.Default.ReplayClip);

    /// <summary>Accepts both the Contracts shape ("bytes") and the research.md 7.6 shape ("base64").</summary>
    public static ThumbData? Thumb(JsonElement d)
    {
        if (d.ValueKind != JsonValueKind.Object) return null;
        string id = d.TryGetProperty("id", out var i) ? i.GetString() ?? "" : "";
        string mime = d.TryGetProperty("mime", out var m) ? m.GetString() ?? "" : "";
        int w = d.TryGetProperty("width", out var wi) ? wi.GetInt32() : 0;
        int h = d.TryGetProperty("height", out var he) ? he.GetInt32() : 0;
        byte[]? bytes = null;
        if (d.TryGetProperty("bytes", out var b) && b.ValueKind == JsonValueKind.String) bytes = b.GetBytesFromBase64();
        else if (d.TryGetProperty("base64", out var b64) && b64.ValueKind == JsonValueKind.String) bytes = b64.GetBytesFromBase64();
        return bytes is null ? null : new ThumbData(id, mime, w, h, bytes);
    }

    public static List<CaptureMeta> Timeline(JsonElement d) => Items(d, ContractsJson.Default.ItemsReplyCaptureMeta);
    public static List<ReplayClip> Replays(JsonElement d) => Items(d, ContractsJson.Default.ItemsReplyReplayClip);
    public static List<AuditEntry> Audit(JsonElement d) => Items(d, ContractsJson.Default.ItemsReplyAuditEntry);
    public static List<HistoryEntry> History(JsonElement d) => Items(d, ContractsJson.Default.ItemsReplyHistoryEntry);
    public static FacetsReply Facets(JsonElement d) => d.Deserialize(ContractsJson.Default.FacetsReply) ?? throw new JsonException();

    private static List<T> Items<T>(JsonElement d, System.Text.Json.Serialization.Metadata.JsonTypeInfo<ItemsReply<T>> info) =>
        d.ValueKind == JsonValueKind.Object && d.Deserialize(info)?.Items is { } items ? items.ToList() : throw new JsonException("items missing");

    public static UnlockReply Unlock(JsonElement d) =>
        d.ValueKind == JsonValueKind.Object ? d.Deserialize(ContractsJson.Default.UnlockReply)! : new UnlockReply(false, null);

    public static PurgeConfirmReply Purge(JsonElement d)
    {
        var r = d.Deserialize(ContractsJson.Default.PurgeConfirmReply);
        return r is { Confirm.Length: > 0 } ? r : throw new JsonException("confirm missing");
    }

    public static int Purged(JsonElement d) => d.GetProperty("purged").GetInt32();

    public static VersionReply Version(JsonElement d) => d.Deserialize(ContractsJson.Default.VersionReply) ?? throw new JsonException();

    public static JsonObject SettingsObject(JsonElement d) =>
        JsonNode.Parse(d.GetRawText()) as JsonObject ?? throw new JsonException("settings is not an object");
}

/// <summary>Typed verbs on top of an IAgentClient. Builds request lines per docs/v1-design.md section 5.</summary>
public sealed class AgentApi(IAgentClient client)
{
    public IAgentClient Client => client;

    public static string Iso(DateTimeOffset t) => t.ToUniversalTime().ToString("yyyy-MM-dd'T'HH:mm:ss'Z'", CultureInfo.InvariantCulture);

    /// <summary>Lowercase JSON form of search (research.md 7.6).</summary>
    public static string BuildSearchLine(SearchQuery q)
    {
        using var ms = new MemoryStream();
        using (var w = new Utf8JsonWriter(ms))
        {
            w.WriteStartObject();
            w.WriteString("q", q.Text.Trim());
            if (q.Limit is { } l) w.WriteNumber("limit", l);
            if (q.From is { } f) w.WriteString("from", Iso(f));
            if (q.To is { } t) w.WriteString("to", Iso(t));
            if (!string.IsNullOrWhiteSpace(q.App)) w.WriteString("app", q.App.Trim());
            if (!string.IsNullOrWhiteSpace(q.Site)) w.WriteString("site", q.Site.Trim());
            if (q.Kind is { } k)
            {
                w.WriteStartArray("kind");
                w.WriteStringValue(k == CaptureKind.Audio ? "audio" : "screen");
                w.WriteEndArray();
            }
            if (q.Mode != SearchMode.Hybrid) w.WriteString("mode", q.Mode.ToString().ToLowerInvariant());
            w.WriteEndObject();
        }
        return "search " + Encoding.UTF8.GetString(ms.ToArray());
    }

    public async Task<AgentResult<VersionReply>> VersionAsync(CancellationToken ct = default) =>
        (await client.SendAsync("version", ct)).As(Responses.Version);

    public async Task<AgentResult<AgentStatus>> StatusAsync(CancellationToken ct = default) =>
        (await client.SendAsync("status", ct)).As(d => Responses.Status(d) ?? throw new JsonException());

    public async Task<AgentResult<SearchResponse>> SearchAsync(SearchQuery q, CancellationToken ct = default) =>
        (await client.SendAsync(BuildSearchLine(q), ct)).As(d => Responses.Search(d) ?? throw new JsonException());

    public async Task<AgentResult<CaptureDetail>> GetAsync(string id, CancellationToken ct = default) =>
        (await client.SendAsync("get " + id, ct)).As(d => Responses.Detail(d) ?? throw new JsonException());

    public async Task<AgentResult<ThumbData>> ThumbAsync(string id, int width = 320, CancellationToken ct = default) =>
        (await client.SendAsync($"thumb {id} {Math.Clamp(width, 64, 1280)}", ct)).As(d => Responses.Thumb(d) ?? throw new JsonException());

    public async Task<AgentResult<ThumbData>> KeyframeAsync(string id, CancellationToken ct = default) =>
        (await client.SendAsync("keyframe " + id, ct)).As(d => Responses.Thumb(d) ?? throw new JsonException());

    public async Task<AgentResult<List<CaptureMeta>>> TimelineAsync(DateTimeOffset from, DateTimeOffset to, int? limit = null, CancellationToken ct = default)
    {
        var line = $"timeline {Iso(from)} {Iso(to)}" + (limit is { } l ? " " + Math.Clamp(l, 1, 5000).ToString(CultureInfo.InvariantCulture) : "");
        return (await client.SendAsync(line, ct)).As(Responses.Timeline);
    }

    /// <summary>Hello when passphrase is null. The caller must not keep the passphrase.</summary>
    public async Task<AgentResult<UnlockReply>> UnlockAsync(string? passphrase = null, CancellationToken ct = default) =>
        (await client.SendAsync(string.IsNullOrEmpty(passphrase) ? "unlock" : "unlock " + passphrase, ct)).As(Responses.Unlock);

    public Task<AgentResult> LockAsync(CancellationToken ct = default) => client.SendAsync("lock", ct);
    public Task<AgentResult> ConsentAsync(CancellationToken ct = default) => client.SendAsync("consent", ct);
    public Task<AgentResult> PauseAsync(int? seconds = null, CancellationToken ct = default) =>
        client.SendAsync(seconds is { } s ? "pause " + s.ToString(CultureInfo.InvariantCulture) : "pause", ct);
    public Task<AgentResult> ResumeAsync(CancellationToken ct = default) => client.SendAsync("resume", ct);
    public Task<AgentResult> PanicAsync(CancellationToken ct = default) => client.SendAsync("panic", ct);

    public async Task<AgentResult<JsonObject>> SettingsGetAsync(CancellationToken ct = default) =>
        (await client.SendAsync("settings get", ct)).As(Responses.SettingsObject);

    public async Task<AgentResult<JsonObject>> SettingsSetAsync(JsonObject partial, CancellationToken ct = default) =>
        (await client.SendAsync("settings set " + partial.ToJsonString(), ct)).As(Responses.SettingsObject);

    public async Task<AgentResult<ReplayClip>> ReplaySaveAsync(CancellationToken ct = default) =>
        (await client.SendAsync("replay save", ct)).As(d => Responses.Replay(d) ?? throw new JsonException());

    public async Task<AgentResult<List<ReplayClip>>> ReplayListAsync(CancellationToken ct = default) =>
        (await client.SendAsync("replay list", ct)).As(Responses.Replays);

    public Task<AgentResult> ReplayExportAsync(string id, string path, CancellationToken ct = default) =>
        client.SendAsync($"replay export {id} {path}", ct);

    public Task<AgentResult> ReplayDeleteAsync(string id, CancellationToken ct = default) => client.SendAsync("replay delete " + id, ct);

    public async Task<AgentResult<PurgeConfirmReply>> PurgeRangeAsync(DateTimeOffset from, DateTimeOffset to, CancellationToken ct = default) =>
        (await client.SendAsync($"purge range {Iso(from)} {Iso(to)}", ct)).As(Responses.Purge);

    public async Task<AgentResult<PurgeConfirmReply>> PurgeAppAsync(string app, CancellationToken ct = default) =>
        (await client.SendAsync("purge app " + app.Trim(), ct)).As(Responses.Purge);

    public async Task<AgentResult<PurgeConfirmReply>> PurgeIdAsync(string id, CancellationToken ct = default) =>
        (await client.SendAsync("purge id " + id.Trim(), ct)).As(Responses.Purge);

    public async Task<AgentResult<PurgeConfirmReply>> PurgeSiteAsync(string site, CancellationToken ct = default) =>
        (await client.SendAsync("purge site " + site.Trim(), ct)).As(Responses.Purge);

    /// <summary>Top apps and sites; both bounds or neither.</summary>
    public async Task<AgentResult<FacetsReply>> FacetsAsync(DateTimeOffset? from, DateTimeOffset? to, CancellationToken ct = default) =>
        (await client.SendAsync(from is { } f && to is { } t ? $"facets {Iso(f)} {Iso(t)}" : "facets", ct)).As(Responses.Facets);

    public async Task<AgentResult<List<HistoryEntry>>> HistoryAsync(CancellationToken ct = default) =>
        (await client.SendAsync("history", ct)).As(Responses.History);

    public Task<AgentResult> HistoryAddAsync(string q, CancellationToken ct = default) => client.SendAsync("history add " + OneLine(q), ct);
    public Task<AgentResult> HistoryDeleteAsync(string q, CancellationToken ct = default) => client.SendAsync("history delete " + OneLine(q), ct);
    public Task<AgentResult> HistoryClearAsync(CancellationToken ct = default) => client.SendAsync("history clear", ct);

    /// <summary>Request lines end at a newline: a query with line breaks is sent as one line.</summary>
    public static string OneLine(string s) => string.Join(' ', s.Split(['\r', '\n'], StringSplitOptions.RemoveEmptyEntries)).Trim();

    public async Task<AgentResult<PurgeConfirmReply>> PurgeAllAsync(CancellationToken ct = default) =>
        (await client.SendAsync("purge all", ct)).As(Responses.Purge);

    public async Task<AgentResult<int>> PurgeConfirmAsync(string token, CancellationToken ct = default) =>
        (await client.SendAsync("purge confirm " + token, ct)).As(Responses.Purged);

    public async Task<AgentResult<List<AuditEntry>>> AuditAsync(int? limit = null, CancellationToken ct = default) =>
        (await client.SendAsync(limit is { } l ? "audit " + l.ToString(CultureInfo.InvariantCulture) : "audit", ct)).As(Responses.Audit);

    /// <summary>Stored full report, or "Not found." when none was run.</summary>
    public async Task<AgentResult<BenchReport>> BenchLastAsync(CancellationToken ct = default) =>
        (await client.SendAsync("bench last", ct)).As(d => d.Deserialize(ContractsJson.Default.BenchReport) ?? throw new JsonException());

    /// <summary>Full benchmark. Can take minutes; cancel closes the connection.</summary>
    public async Task<AgentResult<BenchReport>> BenchFullAsync(CancellationToken ct = default) =>
        (await client.SendAsync("bench full", ct)).As(d => d.Deserialize(ContractsJson.Default.BenchReport) ?? throw new JsonException());

    public async Task<AgentResult<BenchResult>> BenchAsync(CancellationToken ct = default) =>
        (await client.SendAsync("bench", ct)).As(d => Responses.Bench(d) ?? throw new JsonException());
}
