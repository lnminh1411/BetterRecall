// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Text.Json.Nodes;

namespace BetterRecall.Core.Ai;

/// <summary>The one tool a chat model gets: search the user's history (runs locally, results pass ChatShareGate).</summary>
public sealed record ToolSpec(string Name, string Description, JsonObject Parameters);

/// <summary>One provider request. Transcript is the provider-native message list (append-only; earlier entries are
/// never edited, so thinking blocks and thought signatures stay valid).</summary>
public sealed record ChatRequest(string Endpoint, string Model, string? ApiKey, string System, JsonArray Transcript, ToolSpec Tool, int MaxTokens);

public sealed record ToolCall(string Id, string Name, JsonObject Args);

/// <summary>What one tool call returns to the model. Images are already redacted (ChatShareGate).</summary>
public sealed record ToolOutput(string CallId, string Name, string Text, IReadOnlyList<OutgoingImage> Images);

/// <summary>
/// One assistant turn read from a stream. Assistant is the provider-native assistant message exactly as it must be
/// appended to the transcript (all blocks/parts, including thinking blocks, signatures, tool calls).
/// StopReason: end | tool | max_tokens | refusal | context | other.
/// </summary>
public sealed record ProviderTurn(JsonNode Assistant, string Text, IReadOnlyList<ToolCall> Calls, long InputTokens, long OutputTokens, string StopReason);

/// <summary>Wire format of one provider: request building, SSE parsing, transcript appends. No I/O besides the stream.</summary>
public interface IChatProvider
{
    string Name { get; }
    /// <summary>A streaming POST with the key in the provider's header (none when ApiKey is null or empty).</summary>
    HttpRequestMessage BuildRequest(ChatRequest request);
    /// <summary>Reads the SSE body to the end; onText gets each visible text delta. Throws ProviderStreamException on an
    /// error event, InvalidDataException on a malformed stream.</summary>
    Task<ProviderTurn> ReadStreamAsync(Stream body, Action<string> onText, CancellationToken ct);
    void AppendUser(JsonArray transcript, string text);
    void AppendAssistant(JsonArray transcript, ProviderTurn turn);
    /// <summary>Results for every call of the last assistant turn, in call order, as one provider message (plus a
    /// user message with the images where the provider cannot take images in a tool result).</summary>
    void AppendToolResults(JsonArray transcript, IReadOnlyList<ToolOutput> results);
}
