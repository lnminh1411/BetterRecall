// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Net.Http.Headers;
using System.Text;
using System.Text.Json.Nodes;
using BetterRecall.Contracts;

namespace BetterRecall.Core.Ai;

public sealed class OpenAiChat : IChatProvider
{
    public string Name => AiDefaults.OpenAi;

    public HttpRequestMessage BuildRequest(ChatRequest request)
    {
        var endpoint = request.Endpoint.TrimEnd('/');
        var url = $"{endpoint}/chat/completions";
        var req = new HttpRequestMessage(HttpMethod.Post, url);
        req.Headers.TryAddWithoutValidation("Accept", "text/event-stream");
        if (!string.IsNullOrEmpty(request.ApiKey))
        {
            req.Headers.Authorization = new AuthenticationHeaderValue("Bearer", request.ApiKey);
        }

        var messages = new JsonArray(
            new JsonObject
            {
                ["role"] = "system",
                ["content"] = request.System
            }
        );
        foreach (var item in request.Transcript)
        {
            if (item is not null) messages.Add(item.DeepClone());
        }

        var tools = new JsonArray(
            new JsonObject
            {
                ["type"] = "function",
                ["function"] = new JsonObject
                {
                    ["name"] = request.Tool.Name,
                    ["description"] = request.Tool.Description,
                    ["parameters"] = (JsonObject)request.Tool.Parameters.DeepClone()
                }
            }
        );

        var root = new JsonObject
        {
            ["model"] = request.Model,
            ["stream"] = true,
            ["stream_options"] = new JsonObject { ["include_usage"] = true },
            ["messages"] = messages,
            ["tools"] = tools
        };

        req.Content = new StringContent(root.ToJsonString(), Encoding.UTF8, "application/json");
        return req;
    }

    public async Task<ProviderTurn> ReadStreamAsync(Stream body, Action<string> onText, CancellationToken ct)
    {
        var sbText = new StringBuilder();
        var toolCallMap = new SortedDictionary<int, AccumulatedToolCall>();
        string? finishReason = null;
        long inputTokens = 0;
        long outputTokens = 0;

        await foreach (var (_, data) in SseReader.ReadAsync(body, ct).ConfigureAwait(false))
        {
            if (string.IsNullOrWhiteSpace(data)) continue;
            if (data.Trim() == "[DONE]") break;

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
                var code = error["code"] is JsonValue jv ? jv.ToString() : null;
                var errType = error["type"]?.GetValue<string>() ?? code ?? "server_error";
                var errMsg = error["message"]?.GetValue<string>() ?? "";
                throw new ProviderStreamException(errType, errMsg);
            }

            if (chunk["usage"] is JsonObject usage)
            {
                inputTokens = usage["prompt_tokens"]?.GetValue<long>() ?? inputTokens;
                outputTokens = usage["completion_tokens"]?.GetValue<long>() ?? outputTokens;
            }

            if (chunk["choices"] is JsonArray choices && choices.Count > 0 && choices[0] is JsonObject choice0)
            {
                if (choice0["finish_reason"]?.GetValue<string>() is { } fr)
                {
                    finishReason = fr;
                }

                if (choice0["delta"] is JsonObject delta)
                {
                    if (delta["content"]?.GetValue<string>() is { } content)
                    {
                        onText(content);
                        sbText.Append(content);
                    }

                    if (delta["tool_calls"] is JsonArray tcArray)
                    {
                        foreach (var tcNode in tcArray)
                        {
                            if (tcNode is not JsonObject tc) continue;
                            int idx = tc["index"]?.GetValue<int>() ?? 0;
                            if (!toolCallMap.TryGetValue(idx, out var acc))
                            {
                                acc = new AccumulatedToolCall { Index = idx };
                                toolCallMap[idx] = acc;
                            }

                            if (tc["id"]?.GetValue<string>() is { } id && acc.Id is null)
                            {
                                acc.Id = id;
                            }

                            if (tc["function"] is JsonObject fn)
                            {
                                if (fn["name"]?.GetValue<string>() is { } fnName && acc.Name is null)
                                {
                                    acc.Name = fnName;
                                }
                                if (fn["arguments"]?.GetValue<string>() is { } argsFrag)
                                {
                                    acc.Arguments.Append(argsFrag);
                                }
                            }
                        }
                    }
                }
            }
        }

        var calls = new List<ToolCall>();
        foreach (var acc in toolCallMap.Values)
        {
            var id = acc.Id ?? $"call_{acc.Index}";
            var name = acc.Name ?? "";
            JsonObject args;
            var raw = acc.Arguments.ToString();
            if (string.IsNullOrWhiteSpace(raw))
            {
                args = new JsonObject();
            }
            else
            {
                try
                {
                    args = JsonNode.Parse(raw) as JsonObject ?? new JsonObject();
                }
                catch
                {
                    args = new JsonObject();
                }
            }
            calls.Add(new ToolCall(id, name, args));
        }

        string stopReason;
        if (calls.Count > 0)
        {
            stopReason = "tool";
        }
        else
        {
            stopReason = finishReason switch
            {
                "stop" => "end",
                "length" => "max_tokens",
                "content_filter" => "refusal",
                null or "" => sbText.Length > 0 ? "end" : "other",
                _ => "other"
            };
        }

        var assistantNode = new JsonObject
        {
            ["role"] = "assistant",
            ["content"] = sbText.Length > 0 ? sbText.ToString() : null
        };
        if (toolCallMap.Count > 0)
        {
            var tcArray = new JsonArray();
            foreach (var acc in toolCallMap.Values)
            {
                tcArray.Add((JsonNode)new JsonObject
                {
                    ["id"] = acc.Id ?? $"call_{acc.Index}",
                    ["type"] = "function",
                    ["function"] = new JsonObject
                    {
                        ["name"] = acc.Name ?? "",
                        ["arguments"] = acc.Arguments.ToString()
                    }
                });
            }
            assistantNode["tool_calls"] = tcArray;
        }

        return new ProviderTurn(assistantNode, sbText.ToString(), calls, inputTokens, outputTokens, stopReason);
    }

    public void AppendUser(JsonArray transcript, string text)
    {
        transcript.Add((JsonNode)new JsonObject
        {
            ["role"] = "user",
            ["content"] = text
        });
    }

    public void AppendAssistant(JsonArray transcript, ProviderTurn turn)
    {
        transcript.Add(turn.Assistant.DeepClone());
    }

    public void AppendToolResults(JsonArray transcript, IReadOnlyList<ToolOutput> results)
    {
        foreach (var result in results)
        {
            transcript.Add((JsonNode)new JsonObject
            {
                ["role"] = "tool",
                ["tool_call_id"] = result.CallId,
                ["content"] = result.Text
            });
        }

        var hasImages = false;
        foreach (var r in results)
        {
            if (r.Images is { Count: > 0 })
            {
                hasImages = true;
                break;
            }
        }

        if (hasImages)
        {
            var contentArray = new JsonArray(
                new JsonObject
                {
                    ["type"] = "text",
                    ["text"] = "Images for the search results above (redacted)."
                }
            );
            foreach (var r in results)
            {
                if (r.Images != null)
                {
                    foreach (var img in r.Images)
                    {
                        var b64 = Convert.ToBase64String(img.Bytes);
                        contentArray.Add((JsonNode)new JsonObject
                        {
                            ["type"] = "image_url",
                            ["image_url"] = new JsonObject
                            {
                                ["url"] = $"data:{img.Mime};base64,{b64}"
                            }
                        });
                    }
                }
            }
            transcript.Add((JsonNode)new JsonObject
            {
                ["role"] = "user",
                ["content"] = contentArray
            });
        }
    }

    private sealed class AccumulatedToolCall
    {
        public int Index { get; set; }
        public string? Id { get; set; }
        public string? Name { get; set; }
        public StringBuilder Arguments { get; } = new();
    }
}
