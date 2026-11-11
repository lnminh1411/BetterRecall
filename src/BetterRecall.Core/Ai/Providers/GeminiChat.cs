// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Text;
using System.Text.Json.Nodes;
using BetterRecall.Contracts;

namespace BetterRecall.Core.Ai;

public sealed class GeminiChat : IChatProvider
{
    public string Name => AiDefaults.Gemini;

    public HttpRequestMessage BuildRequest(ChatRequest request)
    {
        var endpoint = request.Endpoint.TrimEnd('/');
        var url = $"{endpoint}/v1beta/models/{request.Model}:streamGenerateContent?alt=sse";
        var req = new HttpRequestMessage(HttpMethod.Post, url);
        req.Headers.TryAddWithoutValidation("Accept", "text/event-stream");
        if (!string.IsNullOrEmpty(request.ApiKey))
        {
            req.Headers.TryAddWithoutValidation("x-goog-api-key", request.ApiKey);
        }

        var root = new JsonObject();
        if (request.System != null)
        {
            root["systemInstruction"] = new JsonObject
            {
                ["parts"] = new JsonArray(
                    new JsonObject { ["text"] = request.System }
                )
            };
        }
        root["contents"] = (JsonArray)request.Transcript.DeepClone();
        root["tools"] = new JsonArray(
            new JsonObject
            {
                ["functionDeclarations"] = new JsonArray(
                    new JsonObject
                    {
                        ["name"] = request.Tool.Name,
                        ["description"] = request.Tool.Description,
                        ["parameters"] = (JsonObject)request.Tool.Parameters.DeepClone()
                    }
                )
            }
        );
        root["generationConfig"] = new JsonObject
        {
            ["maxOutputTokens"] = request.MaxTokens
        };

        req.Content = new StringContent(root.ToJsonString(), Encoding.UTF8, "application/json");
        return req;
    }

    public async Task<ProviderTurn> ReadStreamAsync(Stream body, Action<string> onText, CancellationToken ct)
    {
        var sbText = new StringBuilder();
        var keptParts = new List<JsonNode>();
        var toolCalls = new List<ToolCall>();
        var toolCallIndex = 0;
        string? finishReason = null;
        long inputTokens = 0;
        long outputTokens = 0;

        await foreach (var (_, data) in SseReader.ReadAsync(body, ct).ConfigureAwait(false))
        {
            if (string.IsNullOrWhiteSpace(data)) continue;

            JsonNode? parsed;
            try
            {
                parsed = JsonNode.Parse(data);
            }
            catch (Exception ex)
            {
                throw new InvalidDataException("Malformed JSON in stream", ex);
            }
            if (parsed is not JsonObject chunk) continue;

            if (chunk["error"] is JsonObject error)
            {
                var status = error["status"]?.GetValue<string>() ?? "INTERNAL";
                var message = error["message"]?.GetValue<string>() ?? "";
                throw new ProviderStreamException(status, message);
            }

            if (chunk["usageMetadata"] is JsonObject usage)
            {
                inputTokens = usage["promptTokenCount"]?.GetValue<long>() ?? 0;
                var cand = usage["candidatesTokenCount"]?.GetValue<long>() ?? 0;
                var thoughts = usage["thoughtsTokenCount"]?.GetValue<long>() ?? 0;
                outputTokens = cand + thoughts;
            }

            if (chunk["candidates"] is JsonArray candidates && candidates.Count > 0 && candidates[0] is JsonObject cand0)
            {
                if (cand0["finishReason"]?.GetValue<string>() is { } fr)
                {
                    finishReason = fr;
                }

                if (cand0["content"]?["parts"] is JsonArray parts)
                {
                    foreach (var partNode in parts)
                    {
                        if (partNode is null) continue;
                        var clone = partNode.DeepClone();
                        keptParts.Add(clone);

                        if (clone is JsonObject partObj)
                        {
                            var isThought = partObj["thought"]?.GetValue<bool>() ?? false;
                            if (!isThought && partObj["text"]?.GetValue<string>() is { } text)
                            {
                                onText(text);
                                sbText.Append(text);
                            }

                            if (partObj["functionCall"] is JsonObject fc)
                            {
                                var id = fc["id"]?.GetValue<string>();
                                if (string.IsNullOrEmpty(id))
                                {
                                    id = $"gemini-call-{toolCallIndex++}";
                                }
                                var name = fc["name"]?.GetValue<string>() ?? "";
                                var args = fc["args"] is JsonObject argsObj ? (JsonObject)argsObj.DeepClone() : new JsonObject();
                                toolCalls.Add(new ToolCall(id, name, args));
                            }
                        }
                    }
                }
            }
        }

        string stopReason;
        if (toolCalls.Count > 0)
        {
            stopReason = "tool";
        }
        else
        {
            stopReason = finishReason switch
            {
                "STOP" => "end",
                "MAX_TOKENS" => "max_tokens",
                "SAFETY" or "RECITATION" or "BLOCKLIST" or "PROHIBITED_CONTENT" or "SPII" or "IMAGE_SAFETY" => "refusal",
                null or "" => sbText.Length > 0 ? "end" : "other",
                _ => "other"
            };
        }

        var partsArray = new JsonArray();
        foreach (var p in keptParts)
        {
            partsArray.Add(p.DeepClone());
        }
        var assistantNode = new JsonObject
        {
            ["role"] = "model",
            ["parts"] = partsArray
        };

        return new ProviderTurn(assistantNode, sbText.ToString(), toolCalls, inputTokens, outputTokens, stopReason);
    }

    public void AppendUser(JsonArray transcript, string text)
    {
        transcript.Add((JsonNode)new JsonObject
        {
            ["role"] = "user",
            ["parts"] = new JsonArray(
                new JsonObject { ["text"] = text }
            )
        });
    }

    public void AppendAssistant(JsonArray transcript, ProviderTurn turn)
    {
        transcript.Add(turn.Assistant.DeepClone());
    }

    public void AppendToolResults(JsonArray transcript, IReadOnlyList<ToolOutput> results)
    {
        var parts = new JsonArray();
        foreach (var result in results)
        {
            var fr = new JsonObject
            {
                ["name"] = result.Name,
                ["response"] = new JsonObject
                {
                    ["result"] = result.Text
                }
            };
            if (!result.CallId.StartsWith("gemini-call-", StringComparison.Ordinal))
            {
                fr["id"] = result.CallId;
            }
            parts.Add((JsonNode)new JsonObject { ["functionResponse"] = fr });
        }

        foreach (var result in results)
        {
            if (result.Images != null)
            {
                foreach (var img in result.Images)
                {
                    parts.Add((JsonNode)new JsonObject
                    {
                        ["inlineData"] = new JsonObject
                        {
                            ["mimeType"] = img.Mime,
                            ["data"] = Convert.ToBase64String(img.Bytes)
                        }
                    });
                }
            }
        }

        transcript.Add((JsonNode)new JsonObject
        {
            ["role"] = "user",
            ["parts"] = parts
        });
    }
}
