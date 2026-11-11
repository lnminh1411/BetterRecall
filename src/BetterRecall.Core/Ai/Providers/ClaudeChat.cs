// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Text;
using System.Text.Json.Nodes;
using BetterRecall.Contracts;

namespace BetterRecall.Core.Ai;

public sealed class ClaudeChat : IChatProvider
{
    public string Name => AiDefaults.Claude;

    public HttpRequestMessage BuildRequest(ChatRequest request)
    {
        var endpoint = request.Endpoint.TrimEnd('/');
        var url = $"{endpoint}/v1/messages";
        var req = new HttpRequestMessage(HttpMethod.Post, url);
        req.Headers.TryAddWithoutValidation("Accept", "text/event-stream");
        if (!string.IsNullOrEmpty(request.ApiKey))
        {
            req.Headers.TryAddWithoutValidation("x-api-key", request.ApiKey);
        }
        req.Headers.TryAddWithoutValidation("anthropic-version", CloudDescriber.ClaudeVersion);

        var tools = new JsonArray(
            new JsonObject
            {
                ["name"] = request.Tool.Name,
                ["description"] = request.Tool.Description,
                ["input_schema"] = (JsonObject)request.Tool.Parameters.DeepClone()
            }
        );

        var root = new JsonObject
        {
            ["model"] = request.Model,
            ["max_tokens"] = request.MaxTokens,
            ["stream"] = true,
            ["system"] = request.System,
            ["messages"] = (JsonArray)request.Transcript.DeepClone(),
            ["tools"] = tools
        };

        req.Content = new StringContent(root.ToJsonString(), Encoding.UTF8, "application/json");
        return req;
    }

    public async Task<ProviderTurn> ReadStreamAsync(Stream body, Action<string> onText, CancellationToken ct)
    {
        var sbText = new StringBuilder();
        var blocks = new SortedDictionary<int, JsonObject>();
        var jsonBuffers = new Dictionary<int, StringBuilder>();
        string? stopReasonRaw = null;
        long inputTokens = 0;
        long outputTokens = 0;

        await foreach (var (ev, data) in SseReader.ReadAsync(body, ct).ConfigureAwait(false))
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
            if (parsed is not JsonObject root) continue;

            var type = root["type"]?.GetValue<string>();

            if (ev == "error" || type == "error" || root["error"] is not null)
            {
                var errObj = root["error"] as JsonObject;
                var errType = errObj?["type"]?.GetValue<string>() ?? type ?? "api_error";
                if (errType == "error" && errObj?["type"]?.GetValue<string>() is { } subType)
                {
                    errType = subType;
                }
                var errMsg = errObj?["message"]?.GetValue<string>() ?? root["message"]?.GetValue<string>() ?? "";
                throw new ProviderStreamException(errType, errMsg);
            }

            if (type == null || type == "ping") continue;

            switch (type)
            {
                case "message_start":
                    if (root["message"]?["usage"] is JsonObject usage)
                    {
                        inputTokens = usage["input_tokens"]?.GetValue<long>() ?? 0;
                        inputTokens += usage["cache_creation_input_tokens"]?.GetValue<long>() ?? 0;
                        inputTokens += usage["cache_read_input_tokens"]?.GetValue<long>() ?? 0;
                        if (usage["output_tokens"]?.GetValue<long>() is { } startOut)
                        {
                            outputTokens = startOut;
                        }
                    }
                    break;

                case "content_block_start":
                    if (root["index"]?.GetValue<int>() is { } startIdx && root["content_block"] is JsonObject cb)
                    {
                        blocks[startIdx] = (JsonObject)cb.DeepClone();
                    }
                    break;

                case "content_block_delta":
                    if (root["index"]?.GetValue<int>() is { } deltaIdx && root["delta"] is JsonObject delta)
                    {
                        var deltaType = delta["type"]?.GetValue<string>();
                        if (deltaType == "text_delta")
                        {
                            var text = delta["text"]?.GetValue<string>() ?? "";
                            if (blocks.TryGetValue(deltaIdx, out var block))
                            {
                                var cur = block["text"]?.GetValue<string>() ?? "";
                                block["text"] = cur + text;
                            }
                            onText(text);
                            sbText.Append(text);
                        }
                        else if (deltaType == "input_json_delta")
                        {
                            var partial = delta["partial_json"]?.GetValue<string>() ?? "";
                            if (!jsonBuffers.TryGetValue(deltaIdx, out var buf))
                            {
                                buf = new StringBuilder();
                                jsonBuffers[deltaIdx] = buf;
                            }
                            buf.Append(partial);
                        }
                        else if (deltaType == "thinking_delta")
                        {
                            var thinking = delta["thinking"]?.GetValue<string>() ?? "";
                            if (blocks.TryGetValue(deltaIdx, out var block))
                            {
                                var cur = block["thinking"]?.GetValue<string>() ?? "";
                                block["thinking"] = cur + thinking;
                            }
                        }
                        else if (deltaType == "signature_delta")
                        {
                            var sig = delta["signature"]?.GetValue<string>() ?? "";
                            if (blocks.TryGetValue(deltaIdx, out var block))
                            {
                                block["signature"] = sig;
                            }
                        }
                    }
                    break;

                case "content_block_stop":
                    if (root["index"]?.GetValue<int>() is { } stopIdx && blocks.TryGetValue(stopIdx, out var stopBlock))
                    {
                        if (stopBlock["type"]?.GetValue<string>() == "tool_use")
                        {
                            var raw = jsonBuffers.TryGetValue(stopIdx, out var buf) ? buf.ToString() : "";
                            JsonObject parsedInput;
                            if (string.IsNullOrWhiteSpace(raw))
                            {
                                parsedInput = new JsonObject();
                            }
                            else
                            {
                                try
                                {
                                    parsedInput = JsonNode.Parse(raw) as JsonObject ?? new JsonObject();
                                }
                                catch
                                {
                                    parsedInput = new JsonObject();
                                }
                            }
                            stopBlock["input"] = parsedInput;
                        }
                    }
                    break;

                case "message_delta":
                    if (root["delta"]?["stop_reason"]?.GetValue<string>() is { } sr)
                    {
                        stopReasonRaw = sr;
                    }
                    if (root["usage"]?["output_tokens"]?.GetValue<long>() is { } outTok)
                    {
                        outputTokens = outTok;
                    }
                    break;
            }
        }

        var calls = new List<ToolCall>();
        foreach (var block in blocks.Values)
        {
            if (block["type"]?.GetValue<string>() == "tool_use")
            {
                var id = block["id"]?.GetValue<string>() ?? "";
                var name = block["name"]?.GetValue<string>() ?? "";
                var input = block["input"] as JsonObject ?? new JsonObject();
                calls.Add(new ToolCall(id, name, (JsonObject)input.DeepClone()));
            }
        }

        string stopReason = stopReasonRaw switch
        {
            "end_turn" or "stop_sequence" => "end",
            "tool_use" => "tool",
            "max_tokens" => "max_tokens",
            "refusal" => "refusal",
            "model_context_window_exceeded" => "context",
            _ => "other"
        };

        var contentArray = new JsonArray();
        foreach (var block in blocks.Values)
        {
            contentArray.Add(block.DeepClone());
        }
        var assistantNode = new JsonObject
        {
            ["role"] = "assistant",
            ["content"] = contentArray
        };

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
        var contentArray = new JsonArray();
        foreach (var result in results)
        {
            var innerContent = new JsonArray(
                new JsonObject
                {
                    ["type"] = "text",
                    ["text"] = result.Text
                }
            );
            if (result.Images != null)
            {
                foreach (var img in result.Images)
                {
                    var b64 = Convert.ToBase64String(img.Bytes);
                    innerContent.Add((JsonNode)new JsonObject
                    {
                        ["type"] = "image",
                        ["source"] = new JsonObject
                        {
                            ["type"] = "base64",
                            ["media_type"] = img.Mime,
                            ["data"] = b64
                        }
                    });
                }
            }
            contentArray.Add((JsonNode)new JsonObject
            {
                ["type"] = "tool_result",
                ["tool_use_id"] = result.CallId,
                ["content"] = innerContent
            });
        }

        transcript.Add((JsonNode)new JsonObject
        {
            ["role"] = "user",
            ["content"] = contentArray
        });
    }
}
