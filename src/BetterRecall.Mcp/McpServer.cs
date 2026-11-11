// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Buffers;
using System.Text;
using System.Text.Json;

namespace BetterRecall.Mcp;

/// <summary>
/// MCP over stdio, JSON-RPC 2.0, one message per line. Dual-era:
/// - modern (2026-07-28): every request carries params._meta["io.modelcontextprotocol/protocolVersion"]; server/discover; results carry resultType.
/// - legacy (2025-11-25, 2025-06-18, 2025-03-26, 2024-11-05): initialize handshake.
/// Tools: search_history, get_capture, recall_status (research 7.8). Tool results are text.
/// </summary>
public sealed class McpServer(Func<string, CancellationToken, Task<PipeAnswer>> pipe)
{
    public const string Modern = "2026-07-28";
    public static readonly string[] LegacyVersions = ["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];
    private const string MetaVersion = "io.modelcontextprotocol/protocolVersion";
    private const string Instructions = "Searches the user's local BetterRecall history (screen keyframes by window title, app, on-screen text and image meaning; audio transcripts). Results are private data of the user.";

    private const string ToolsJson = """
        [
          {"name":"search_history","title":"Search BetterRecall history","description":"Search what the user saw or heard on this PC. Hybrid keyword and semantic search over window titles, app names, on-screen text, keyframe images and audio transcripts. Returns JSON with results (id, ts, kind, app, title, score, snippet).",
           "inputSchema":{"type":"object","properties":{
             "q":{"type":"string","description":"What to look for, natural language or keywords. 1 to 1024 characters.","minLength":1,"maxLength":1024},
             "limit":{"type":"integer","description":"Maximum results.","minimum":1},
             "from":{"type":"string","format":"date-time","description":"Only results at or after this ISO 8601 time."},
             "to":{"type":"string","format":"date-time","description":"Only results before this ISO 8601 time."},
             "app":{"type":"string","description":"Process image name, for example chrome.exe."},
             "kind":{"type":"array","items":{"type":"string","enum":["screen","audio"]},"description":"Capture kinds to include."},
             "mode":{"type":"string","enum":["hybrid","text","vector"],"description":"Search mode, default hybrid."}},
             "required":["q"],"additionalProperties":false},
           "annotations":{"readOnlyHint":true,"openWorldHint":false}},
          {"name":"get_capture","title":"Get one capture","description":"Full stored text of one search result: the on-screen text (private details removed) for screen captures, or the window title when no text was read; the transcript for audio.",
           "inputSchema":{"type":"object","properties":{"id":{"type":"string","description":"Result id from search_history."}},"required":["id"],"additionalProperties":false},
           "annotations":{"readOnlyHint":true,"openWorldHint":false}},
          {"name":"recall_status","title":"BetterRecall status","description":"Whether BetterRecall is capturing, paused or locked, and store statistics.",
           "inputSchema":{"type":"object","additionalProperties":false},
           "annotations":{"readOnlyHint":true,"openWorldHint":false}}
        ]
        """;

    private static readonly JsonDocument Tools = JsonDocument.Parse(ToolsJson);
    private string? _legacyVersion;

    /// <summary>Handles one input line. Returns the response line, or null for notifications.</summary>
    public async Task<string?> HandleAsync(string line, CancellationToken ct)
    {
        JsonDocument doc;
        try { doc = JsonDocument.Parse(line); }
        catch (JsonException) { return Error(null, -32700, "Parse error"); }
        using (doc)
        {
            var root = doc.RootElement;
            if (root.ValueKind != JsonValueKind.Object) return Error(null, -32600, "Invalid Request");
            root.TryGetProperty("id", out var idEl);
            bool hasId = idEl.ValueKind is JsonValueKind.String or JsonValueKind.Number;
            JsonElement? id = hasId ? idEl.Clone() : null;
            if (!root.TryGetProperty("method", out var m) || m.ValueKind != JsonValueKind.String)
                return hasId || idEl.ValueKind == JsonValueKind.Undefined ? null : Error(null, -32600, "Invalid Request");
            var method = m.GetString()!;
            if (!hasId) return null; // notification: notifications/initialized, notifications/cancelled, ...
            root.TryGetProperty("params", out var p);

            string? modernVersion = null;
            if (p.ValueKind == JsonValueKind.Object && p.TryGetProperty("_meta", out var meta) && meta.ValueKind == JsonValueKind.Object
                && meta.TryGetProperty(MetaVersion, out var v) && v.ValueKind == JsonValueKind.String)
                modernVersion = v.GetString();
            bool modern = modernVersion != null;
            if (modern && modernVersion != Modern) return UnsupportedVersion(id, modernVersion!);

            switch (method)
            {
                case "initialize":
                    return Initialize(id, p);
                case "server/discover":
                    return Result(id, w =>
                    {
                        w.WriteString("resultType", "complete");
                        w.WriteStartArray("supportedVersions");
                        w.WriteStringValue(Modern);
                        foreach (var lv in LegacyVersions) w.WriteStringValue(lv);
                        w.WriteEndArray();
                        Capabilities(w);
                        w.WriteStartObject("_meta");
                        w.WritePropertyName("io.modelcontextprotocol/serverInfo");
                        ServerInfo(w);
                        w.WriteEndObject();
                        w.WriteString("instructions", Instructions);
                    });
                case "ping":
                    return Result(id, w => { if (modern) w.WriteString("resultType", "complete"); });
                case "tools/list":
                    return Result(id, w =>
                    {
                        if (modern) w.WriteString("resultType", "complete");
                        w.WritePropertyName("tools");
                        Tools.RootElement.WriteTo(w); // compact: stdio framing is one message per line
                    });
                case "tools/call":
                    return await CallAsync(id, p, modern, ct).ConfigureAwait(false);
                default:
                    return Error(id, -32601, "Method not found");
            }
        }
    }

    private string Initialize(JsonElement? id, JsonElement p)
    {
        string requested = p.ValueKind == JsonValueKind.Object && p.TryGetProperty("protocolVersion", out var pv) && pv.ValueKind == JsonValueKind.String ? pv.GetString()! : "";
        _legacyVersion = LegacyVersions.Contains(requested) ? requested : LegacyVersions[0];
        return Result(id, w =>
        {
            w.WriteString("protocolVersion", _legacyVersion);
            Capabilities(w);
            w.WritePropertyName("serverInfo");
            ServerInfo(w);
            w.WriteString("instructions", Instructions);
        });
    }

    private static void Capabilities(Utf8JsonWriter w)
    {
        w.WriteStartObject("capabilities");
        w.WriteStartObject("tools");
        w.WriteBoolean("listChanged", false);
        w.WriteEndObject();
        w.WriteEndObject();
    }

    private static void ServerInfo(Utf8JsonWriter w)
    {
        w.WriteStartObject();
        w.WriteString("name", "betterrecall");
        w.WriteString("title", "BetterRecall");
        w.WriteString("version", BetterRecall.Core.Ipc.CommandProcessor.AppVersion);
        w.WriteEndObject();
    }

    private async Task<string> CallAsync(JsonElement? id, JsonElement p, bool modern, CancellationToken ct)
    {
        if (p.ValueKind != JsonValueKind.Object || !p.TryGetProperty("name", out var n) || n.ValueKind != JsonValueKind.String)
            return Error(id, -32602, "Invalid params");
        JsonElement args = p.TryGetProperty("arguments", out var a) && a.ValueKind == JsonValueKind.Object ? a : default;
        string request;
        switch (n.GetString())
        {
            case "search_history":
                if (args.ValueKind != JsonValueKind.Object || !args.TryGetProperty("q", out var q) || q.ValueKind != JsonValueKind.String)
                    return ToolResult(id, modern, "Missing q.", true);
                request = "search " + SearchJson(args);
                break;
            case "get_capture":
                if (args.ValueKind != JsonValueKind.Object || !args.TryGetProperty("id", out var cid) || cid.ValueKind != JsonValueKind.String
                    || string.IsNullOrWhiteSpace(cid.GetString()) || cid.GetString()!.Any(c => char.IsWhiteSpace(c) || char.IsControl(c)))
                    return ToolResult(id, modern, "Missing or bad id.", true);
                request = "get " + cid.GetString();
                break;
            case "recall_status":
                request = "status";
                break;
            default:
                return Error(id, -32602, "Unknown tool: " + n.GetString());
        }
        if (Encoding.UTF8.GetByteCount(request) > 4000) return ToolResult(id, modern, "Request too long.", true);
        var answer = await pipe(request, ct).ConfigureAwait(false);
        return ToolResult(id, modern, answer.Text, !answer.Ok);
    }

    /// <summary>Copies only the known search fields, so nothing else reaches the pipe.</summary>
    internal static string SearchJson(JsonElement args)
    {
        var buf = new ArrayBufferWriter<byte>();
        using (var w = new Utf8JsonWriter(buf))
        {
            w.WriteStartObject();
            foreach (var name in new[] { "q", "limit", "from", "to", "app", "kind", "mode" })
                if (args.TryGetProperty(name, out var v)) { w.WritePropertyName(name); v.WriteTo(w); }
            w.WriteEndObject();
        }
        return Encoding.UTF8.GetString(buf.WrittenSpan);
    }

    private static string ToolResult(JsonElement? id, bool modern, string text, bool isError) => Result(id, w =>
    {
        if (modern) w.WriteString("resultType", "complete");
        w.WriteStartArray("content");
        w.WriteStartObject();
        w.WriteString("type", "text");
        w.WriteString("text", text);
        w.WriteEndObject();
        w.WriteEndArray();
        w.WriteBoolean("isError", isError);
    });

    private static string UnsupportedVersion(JsonElement? id, string requested) => Write(w =>
    {
        WriteHead(w, id);
        w.WriteStartObject("error");
        w.WriteNumber("code", -32022);
        w.WriteString("message", "Unsupported protocol version");
        w.WriteStartObject("data");
        w.WriteStartArray("supported");
        w.WriteStringValue(Modern);
        foreach (var lv in LegacyVersions) w.WriteStringValue(lv);
        w.WriteEndArray();
        w.WriteString("requested", requested);
        w.WriteEndObject();
        w.WriteEndObject();
    });

    private static string Result(JsonElement? id, Action<Utf8JsonWriter> body) => Write(w =>
    {
        WriteHead(w, id);
        w.WriteStartObject("result");
        body(w);
        w.WriteEndObject();
    });

    private static string Error(JsonElement? id, int code, string message) => Write(w =>
    {
        WriteHead(w, id);
        w.WriteStartObject("error");
        w.WriteNumber("code", code);
        w.WriteString("message", message);
        w.WriteEndObject();
    });

    private static void WriteHead(Utf8JsonWriter w, JsonElement? id)
    {
        w.WriteString("jsonrpc", "2.0");
        w.WritePropertyName("id");
        if (id is { } i) i.WriteTo(w); else w.WriteNullValue();
    }

    private static string Write(Action<Utf8JsonWriter> body)
    {
        var buf = new ArrayBufferWriter<byte>();
        using (var w = new Utf8JsonWriter(buf, new JsonWriterOptions { Encoder = System.Text.Encodings.Web.JavaScriptEncoder.UnsafeRelaxedJsonEscaping }))
        {
            w.WriteStartObject();
            body(w);
            w.WriteEndObject();
        }
        return Encoding.UTF8.GetString(buf.WrittenSpan);
    }
}

public readonly record struct PipeAnswer(bool Ok, string Text);
