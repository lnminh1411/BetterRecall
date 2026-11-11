// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Text.Json;
using System.Text.Json.Nodes;
using BetterRecall.Contracts;

namespace BetterRecall.Core.Ipc;

/// <summary>
/// chat (UI pipe only): cloud chat over the user's history (Ai.ChatService). send / approve / deny reply with several
/// lines, each {"ok":true,"data":ChatEvent}, ending with a done, approval or error event; errors found before the
/// turn starts (cloud off, no key, locked, bad JSON) are one ordinary error line.
/// </summary>
public sealed partial class CommandProcessor
{
    private Reply ChatVerb(ClientSession s, string rest, CancellationToken ct)
    {
        var chat = _chat;
        var (sub, tail) = SplitVerb(rest);
        sub = sub.ToLowerInvariant();
        if (chat == null) return Err(ErrAiOff);
        if (IsLocked(s.Kind)) return Err(ErrLocked);
        switch (sub)
        {
            case "list" when tail.Length == 0:
                return Ok(JsonSerializer.Serialize(new ItemsReply<ChatSummary>(chat.List(200)), CoreJson.Default.ItemsReplyChatSummary));
            case "get" when tail.Length > 0:
            {
                var detail = chat.Get(tail.Trim());
                return detail == null ? Err(ErrNotFound) : Ok(JsonSerializer.Serialize(detail, CoreJson.Default.ChatDetail));
            }
            case "rename" when tail.Length > 0:
            {
                if (ParseObject(tail) is not { } o || Str(o, "id") is not { Length: > 0 } id || Str(o, "title") is not { } title) return Err(ErrJson);
                return chat.Rename(id, title) ? Ok("null") : Err(ErrNotFound);
            }
            case "delete" when tail.Length > 0:
            {
                int n = chat.Delete(tail.Trim());
                Audit(s, "chat delete", n, "");
                return n > 0 ? Ok("null") : Err(ErrNotFound);
            }
            case "clear" when tail.Length == 0:
                Audit(s, "chat clear", chat.Delete(null), "");
                return Ok("null");
            case "send" or "approve" or "deny" when tail.Length > 0:
            {
                var ai = S.Ai;
                if (!ai.Enabled) return Err(ErrAiOff);
                if (ai.Provider != AiDefaults.OpenAi && _d.AiKey?.IsSet != true) return Err(ErrNoApiKey);
                Action<string, int, string> audit = (verb, n, sha) => Audit(s, verb, n, sha);
                if (sub == "deny")
                {
                    var token = tail.Trim();
                    return Streaming(emit => chat.DenyAsync(token, emit, ct, audit));
                }
                if (ParseObject(tail) is not { } o) return Err(ErrJson);
                if (sub == "send")
                {
                    var chatId = Str(o, "chat") is { Length: > 0 } c ? c : null;
                    var text = Str(o, "text") ?? "";
                    return Streaming(emit => chat.SendAsync(chatId, text, emit, ct, audit));
                }
                if (Str(o, "token") is not { Length: > 0 } tok) return Err(ErrJson);
                var remove = o["remove"] is JsonArray arr
                    ? arr.Select(n => n is JsonValue v && v.TryGetValue<string>(out var id) ? id : null).OfType<string>().ToHashSet(StringComparer.Ordinal)
                    : [];
                return Streaming(emit => chat.ApproveAsync(tok, remove, emit, ct, audit));
            }
        }
        return Err(UsageChat);
    }

    private static Reply Streaming(Func<Action<Contracts.ChatEvent>, Task> turn) =>
        new("", Stream: line => turn(e => line(Envelope.Ok(JsonSerializer.Serialize(e, CoreJson.Default.ChatEvent)))));

    private static JsonObject? ParseObject(string json)
    {
        try { return JsonNode.Parse(json) as JsonObject; }
        catch (JsonException) { return null; }
    }

    private static string? Str(JsonObject o, string name)
    {
        try { return o[name] is JsonValue v && v.TryGetValue<string>(out var s) ? s : null; }
        catch (InvalidOperationException) { return null; }
    }
}
