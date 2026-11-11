// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Text;
using System.Text.Json.Nodes;
using BetterRecall.Contracts;
using BetterRecall.Core.Ai;

namespace BetterRecall.Core.Tests;

public sealed class ChatProviderTests
{
    private static ToolSpec HistoryTool() => new(
        "search_history",
        "Search user history",
        new JsonObject
        {
            ["type"] = "object",
            ["properties"] = new JsonObject
            {
                ["query"] = new JsonObject { ["type"] = "string", ["description"] = "Search query" }
            },
            ["required"] = new JsonArray { "query" }
        });

    private static MemoryStream StreamOf(string sse) => new(Encoding.UTF8.GetBytes(sse));

    [Fact]
    public void BuildRequest_Gemini()
    {
        var provider = new GeminiChat();
        var tool = HistoryTool();
        var transcript = new JsonArray
        {
            new JsonObject
            {
                ["role"] = "user",
                ["parts"] = new JsonArray { new JsonObject { ["text"] = "hello" } }
            }
        };
        var reqWithKey = new ChatRequest("https://generativelanguage.googleapis.com", "gemini-3.8-flash", "test-gemini-key", "You are helpful.", transcript, tool, 2048);

        using var httpReq = provider.BuildRequest(reqWithKey);

        Assert.Equal(HttpMethod.Post, httpReq.Method);
        Assert.Equal("https://generativelanguage.googleapis.com/v1beta/models/gemini-3.8-flash:streamGenerateContent?alt=sse", httpReq.RequestUri!.ToString());
        Assert.DoesNotContain("test-gemini-key", httpReq.RequestUri.ToString());
        Assert.True(httpReq.Headers.TryGetValues("x-goog-api-key", out var keys));
        Assert.Equal("test-gemini-key", keys.First());
        Assert.True(httpReq.Headers.TryGetValues("Accept", out var accepts));
        Assert.Contains("text/event-stream", accepts);

        var content = httpReq.Content!.ReadAsStringAsync().Result;
        var json = JsonNode.Parse(content)!.AsObject();
        Assert.Equal("You are helpful.", json["systemInstruction"]?["parts"]?[0]?["text"]?.GetValue<string>());
        Assert.Equal(1, json["contents"]?.AsArray().Count);
        Assert.Equal("search_history", json["tools"]?[0]?["functionDeclarations"]?[0]?["name"]?.GetValue<string>());
        Assert.Equal(2048, json["generationConfig"]?["maxOutputTokens"]?.GetValue<int>());
        Assert.Null(json["generationConfig"]?["temperature"]);

        // Absent key header when null or empty
        var reqNoKey = new ChatRequest("https://generativelanguage.googleapis.com", "gemini-3.8-flash", null, "Sys", transcript, tool, 100);
        using var httpReqNoKey = provider.BuildRequest(reqNoKey);
        Assert.False(httpReqNoKey.Headers.Contains("x-goog-api-key"));

        var reqEmptyKey = new ChatRequest("https://generativelanguage.googleapis.com", "gemini-3.8-flash", "", "Sys", transcript, tool, 100);
        using var httpReqEmptyKey = provider.BuildRequest(reqEmptyKey);
        Assert.False(httpReqEmptyKey.Headers.Contains("x-goog-api-key"));
    }

    [Fact]
    public void BuildRequest_OpenAi()
    {
        var provider = new OpenAiChat();
        var tool = HistoryTool();
        var transcript = new JsonArray
        {
            new JsonObject { ["role"] = "user", ["content"] = "previous message" }
        };
        var reqWithKey = new ChatRequest("https://api.openai.com/v1", "gpt-6-luna", "test-openai-key", "You are helpful.", transcript, tool, 2048);

        using var httpReq = provider.BuildRequest(reqWithKey);

        Assert.Equal(HttpMethod.Post, httpReq.Method);
        Assert.Equal("https://api.openai.com/v1/chat/completions", httpReq.RequestUri!.ToString());
        Assert.Equal("Bearer", httpReq.Headers.Authorization?.Scheme);
        Assert.Equal("test-openai-key", httpReq.Headers.Authorization?.Parameter);
        Assert.True(httpReq.Headers.TryGetValues("Accept", out var accepts));
        Assert.Contains("text/event-stream", accepts);

        var content = httpReq.Content!.ReadAsStringAsync().Result;
        var json = JsonNode.Parse(content)!.AsObject();
        Assert.Equal("gpt-6-luna", json["model"]?.GetValue<string>());
        Assert.True(json["stream"]?.GetValue<bool>());
        Assert.True(json["stream_options"]?["include_usage"]?.GetValue<bool>());

        var msgs = json["messages"]!.AsArray();
        Assert.Equal(2, msgs.Count);
        Assert.Equal("system", msgs[0]?["role"]?.GetValue<string>());
        Assert.Equal("You are helpful.", msgs[0]?["content"]?.GetValue<string>());
        Assert.Equal("user", msgs[1]?["role"]?.GetValue<string>());

        var tools = json["tools"]!.AsArray();
        Assert.Equal("function", tools[0]?["type"]?.GetValue<string>());
        Assert.Equal("search_history", tools[0]?["function"]?["name"]?.GetValue<string>());

        // OpenAI must not have max_tokens or max_completion_tokens or temperature
        Assert.Null(json["max_tokens"]);
        Assert.Null(json["max_completion_tokens"]);
        Assert.Null(json["temperature"]);

        // Absent key header when null or empty
        var reqNoKey = new ChatRequest("https://api.openai.com/v1", "gpt-6-luna", null, "Sys", transcript, tool, 100);
        using var httpReqNoKey = provider.BuildRequest(reqNoKey);
        Assert.Null(httpReqNoKey.Headers.Authorization);

        var reqEmptyKey = new ChatRequest("https://api.openai.com/v1", "gpt-6-luna", "", "Sys", transcript, tool, 100);
        using var httpReqEmptyKey = provider.BuildRequest(reqEmptyKey);
        Assert.Null(httpReqEmptyKey.Headers.Authorization);
    }

    [Fact]
    public void BuildRequest_Claude()
    {
        var provider = new ClaudeChat();
        var tool = HistoryTool();
        var transcript = new JsonArray
        {
            new JsonObject { ["role"] = "user", ["content"] = "previous message" }
        };
        var reqWithKey = new ChatRequest("https://api.anthropic.com", "claude-haiku-5-5", "test-claude-key", "You are helpful.", transcript, tool, 1500);

        using var httpReq = provider.BuildRequest(reqWithKey);

        Assert.Equal(HttpMethod.Post, httpReq.Method);
        Assert.Equal("https://api.anthropic.com/v1/messages", httpReq.RequestUri!.ToString());
        Assert.True(httpReq.Headers.TryGetValues("x-api-key", out var keys));
        Assert.Equal("test-claude-key", keys.First());
        Assert.True(httpReq.Headers.TryGetValues("anthropic-version", out var versions));
        Assert.Equal(CloudDescriber.ClaudeVersion, versions.First());
        Assert.True(httpReq.Headers.TryGetValues("Accept", out var accepts));
        Assert.Contains("text/event-stream", accepts);

        var content = httpReq.Content!.ReadAsStringAsync().Result;
        var json = JsonNode.Parse(content)!.AsObject();
        Assert.Equal("claude-haiku-5-5", json["model"]?.GetValue<string>());
        Assert.Equal(1500, json["max_tokens"]?.GetValue<int>());
        Assert.True(json["stream"]?.GetValue<bool>());
        Assert.Equal("You are helpful.", json["system"]?.GetValue<string>());
        Assert.Equal(1, json["messages"]?.AsArray().Count);

        var tools = json["tools"]!.AsArray();
        Assert.Equal("search_history", tools[0]?["name"]?.GetValue<string>());
        Assert.NotNull(tools[0]?["input_schema"]);

        // Must not have forbidden fields
        Assert.Null(json["temperature"]);
        Assert.Null(json["top_p"]);
        Assert.Null(json["top_k"]);
        Assert.Null(json["thinking"]);
        Assert.Null(json["tool_choice"]);

        // Absent key header when null or empty
        var reqNoKey = new ChatRequest("https://api.anthropic.com", "claude-haiku-5-5", null, "Sys", transcript, tool, 100);
        using var httpReqNoKey = provider.BuildRequest(reqNoKey);
        Assert.False(httpReqNoKey.Headers.Contains("x-api-key"));

        var reqEmptyKey = new ChatRequest("https://api.anthropic.com", "claude-haiku-5-5", "", "Sys", transcript, tool, 100);
        using var httpReqEmptyKey = provider.BuildRequest(reqEmptyKey);
        Assert.False(httpReqEmptyKey.Headers.Contains("x-api-key"));
    }

    [Fact]
    public async Task Gemini_Stream_Text_Thought_ToolCall_Usage()
    {
        var provider = new GeminiChat();
        var sse =
            """
            data: {"candidates":[{"content":{"parts":[{"text":"Thinking step 1...","thought":true}]}}]}

            data: {"candidates":[{"content":{"parts":[{"text":"Searching for "}]}}]}

            data: {"candidates":[{"content":{"parts":[{"text":"your files."}]}}]}

            data: {"candidates":[{"content":{"parts":[{"functionCall":{"name":"search_history","args":{"query":"betterrecall"}},"thoughtSignature":"sig_gemini_xyz"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":15,"candidatesTokenCount":10,"thoughtsTokenCount":5}}

            """;

        var textChunks = new List<string>();
        using var stream = StreamOf(sse);
        var turn = await provider.ReadStreamAsync(stream, textChunks.Add, CancellationToken.None);

        // onText received only visible text (thought was omitted)
        Assert.Equal(["Searching for ", "your files."], textChunks);
        Assert.Equal("Searching for your files.", turn.Text);

        // 1 tool call with synthesized id
        Assert.Single(turn.Calls);
        Assert.Equal("gemini-call-0", turn.Calls[0].Id);
        Assert.Equal("search_history", turn.Calls[0].Name);
        Assert.Equal("betterrecall", turn.Calls[0].Args["query"]?.GetValue<string>());

        // StopReason "tool" because call exists, even with finishReason STOP
        Assert.Equal("tool", turn.StopReason);

        // Tokens summed
        Assert.Equal(15, turn.InputTokens);
        Assert.Equal(15, turn.OutputTokens); // 10 candidates + 5 thoughts

        // Assistant parts kept in arrival order, thoughtSignature preserved
        var assistant = turn.Assistant.AsObject();
        Assert.Equal("model", assistant["role"]?.GetValue<string>());
        var parts = assistant["parts"]!.AsArray();
        Assert.Equal(4, parts.Count);
        Assert.True(parts[0]?["thought"]?.GetValue<bool>());
        Assert.Equal("sig_gemini_xyz", parts[3]?["thoughtSignature"]?.GetValue<string>());
    }

    [Fact]
    public async Task Gemini_Stream_ErrorChunk_Throws()
    {
        var provider = new GeminiChat();
        var sse =
            """
            data: {"error":{"code":429,"status":"RESOURCE_EXHAUSTED","message":"Quota limit exceeded for current project."}}

            """;

        using var stream = StreamOf(sse);
        var ex = await Assert.ThrowsAsync<ProviderStreamException>(() =>
            provider.ReadStreamAsync(stream, _ => { }, CancellationToken.None));

        Assert.Equal("RESOURCE_EXHAUSTED", ex.Type);
        Assert.Contains("Quota limit exceeded", ex.Message);
    }

    [Fact]
    public async Task OpenAi_Stream_Deltas_ToolCall_Usage_Done()
    {
        var provider = new OpenAiChat();
        var sse =
            """
            data: {"choices":[{"delta":{"content":"Querying "}}]}

            data: {"choices":[{"delta":{"content":"database..."}}]}

            data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_98765","function":{"name":"search_history","arguments":"{\"qu"}}]}}]}

            data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"ery\":\"hello\"}"}}]}}]}

            data: {"choices":[{"delta":{},"finish_reason":"tool_calls"}]}

            data: {"choices":[],"usage":{"prompt_tokens":22,"completion_tokens":33}}

            data: [DONE]

            """;

        var textChunks = new List<string>();
        using var stream = StreamOf(sse);
        var turn = await provider.ReadStreamAsync(stream, textChunks.Add, CancellationToken.None);

        Assert.Equal(["Querying ", "database..."], textChunks);
        Assert.Equal("Querying database...", turn.Text);

        Assert.Single(turn.Calls);
        Assert.Equal("call_98765", turn.Calls[0].Id);
        Assert.Equal("search_history", turn.Calls[0].Name);
        Assert.Equal("hello", turn.Calls[0].Args["query"]?.GetValue<string>());

        Assert.Equal("tool", turn.StopReason);
        Assert.Equal(22, turn.InputTokens);
        Assert.Equal(33, turn.OutputTokens);

        var assistant = turn.Assistant.AsObject();
        Assert.Equal("assistant", assistant["role"]?.GetValue<string>());
        Assert.Equal("Querying database...", assistant["content"]?.GetValue<string>());

        var tc = assistant["tool_calls"]!.AsArray();
        Assert.Single(tc);
        Assert.Equal("call_98765", tc[0]?["id"]?.GetValue<string>());
        Assert.Equal("function", tc[0]?["type"]?.GetValue<string>());
        Assert.Equal("search_history", tc[0]?["function"]?["name"]?.GetValue<string>());
        Assert.Equal("{\"query\":\"hello\"}", tc[0]?["function"]?["arguments"]?.GetValue<string>());
    }

    [Fact]
    public async Task Claude_Stream_Thinking_Text_ToolUse()
    {
        var provider = new ClaudeChat();
        var sse =
            """
            event: message_start
            data: {"type":"message_start","message":{"usage":{"input_tokens":10,"cache_creation_input_tokens":2,"cache_read_input_tokens":3,"output_tokens":1}}}

            event: content_block_start
            data: {"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}

            event: content_block_delta
            data: {"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"Let me analyze the request"}}

            event: content_block_delta
            data: {"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig_claude_123"}}

            event: content_block_stop
            data: {"type":"content_block_stop","index":0}

            event: content_block_start
            data: {"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}

            event: content_block_delta
            data: {"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"I will search for that."}}

            event: content_block_stop
            data: {"type":"content_block_stop","index":1}

            event: content_block_start
            data: {"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"toolu_01ABC","name":"search_history","input":{}}}

            event: content_block_delta
            data: {"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":""}}

            event: content_block_delta
            data: {"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{\"query\":\"anthropic\"}"}}

            event: content_block_stop
            data: {"type":"content_block_stop","index":2}

            event: message_delta
            data: {"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":42}}

            """;

        var textChunks = new List<string>();
        using var stream = StreamOf(sse);
        var turn = await provider.ReadStreamAsync(stream, textChunks.Add, CancellationToken.None);

        Assert.Equal(["I will search for that."], textChunks);
        Assert.Equal("I will search for that.", turn.Text);

        Assert.Single(turn.Calls);
        Assert.Equal("toolu_01ABC", turn.Calls[0].Id);
        Assert.Equal("search_history", turn.Calls[0].Name);
        Assert.Equal("anthropic", turn.Calls[0].Args["query"]?.GetValue<string>());

        Assert.Equal("tool", turn.StopReason);
        Assert.Equal(15, turn.InputTokens); // 10 + 2 + 3
        Assert.Equal(42, turn.OutputTokens);

        var assistant = turn.Assistant.AsObject();
        Assert.Equal("assistant", assistant["role"]?.GetValue<string>());
        var blocks = assistant["content"]!.AsArray();
        Assert.Equal(3, blocks.Count);

        // Block 0: thinking block with thinking and signature
        Assert.Equal("thinking", blocks[0]?["type"]?.GetValue<string>());
        Assert.Equal("Let me analyze the request", blocks[0]?["thinking"]?.GetValue<string>());
        Assert.Equal("sig_claude_123", blocks[0]?["signature"]?.GetValue<string>());

        // Block 1: text block
        Assert.Equal("text", blocks[1]?["type"]?.GetValue<string>());
        Assert.Equal("I will search for that.", blocks[1]?["text"]?.GetValue<string>());

        // Block 2: tool_use block
        Assert.Equal("tool_use", blocks[2]?["type"]?.GetValue<string>());
        Assert.Equal("toolu_01ABC", blocks[2]?["id"]?.GetValue<string>());
        Assert.Equal("anthropic", blocks[2]?["input"]?["query"]?.GetValue<string>());
    }

    [Fact]
    public async Task Claude_Stream_ErrorEvent_Throws()
    {
        var provider = new ClaudeChat();
        var sse =
            """
            event: error
            data: {"type":"error","error":{"type":"overloaded_error","message":"Anthropic is overloaded right now."}}

            """;

        using var stream = StreamOf(sse);
        var ex = await Assert.ThrowsAsync<ProviderStreamException>(() =>
            provider.ReadStreamAsync(stream, _ => { }, CancellationToken.None));

        Assert.Equal("overloaded_error", ex.Type);
        Assert.Contains("Anthropic is overloaded", ex.Message);
    }

    [Fact]
    public void AppendToolResults_Gemini()
    {
        var provider = new GeminiChat();
        var transcript = new JsonArray();
        var img = new OutgoingImage([1, 2, 3, 4], "image/png", 10, 10, true);

        // One result with synthesized id "gemini-call-0" and one image
        var results = new List<ToolOutput>
        {
            new("gemini-call-0", "search_history", "Found 1 match", [img])
        };

        provider.AppendToolResults(transcript, results);

        Assert.Single(transcript);
        var item = transcript[0]!.AsObject();
        Assert.Equal("user", item["role"]?.GetValue<string>());
        var parts = item["parts"]!.AsArray();
        Assert.Equal(2, parts.Count);

        // First part: functionResponse with omitted id
        var fr = parts[0]?["functionResponse"]?.AsObject();
        Assert.NotNull(fr);
        Assert.Equal("search_history", fr["name"]?.GetValue<string>());
        Assert.Equal("Found 1 match", fr["response"]?["result"]?.GetValue<string>());
        Assert.Null(fr["id"]); // omitted because starts with "gemini-call-"

        // Second part: inlineData
        var inline = parts[1]?["inlineData"]?.AsObject();
        Assert.NotNull(inline);
        Assert.Equal("image/png", inline["mimeType"]?.GetValue<string>());
        Assert.Equal(Convert.ToBase64String([1, 2, 3, 4]), inline["data"]?.GetValue<string>());

        // If CallId does not start with "gemini-call-", id is included
        var transcript2 = new JsonArray();
        var results2 = new List<ToolOutput>
        {
            new("custom_call_42", "search_history", "Found 2 matches", [])
        };
        provider.AppendToolResults(transcript2, results2);
        var fr2 = transcript2[0]?["parts"]?[0]?["functionResponse"]?.AsObject();
        Assert.NotNull(fr2);
        Assert.Equal("custom_call_42", fr2["id"]?.GetValue<string>());
    }

    [Fact]
    public void AppendToolResults_OpenAi()
    {
        var provider = new OpenAiChat();
        var transcript = new JsonArray();
        var img = new OutgoingImage([10, 20, 30], "image/jpeg", 20, 20, true);

        var results = new List<ToolOutput>
        {
            new("call_openai_1", "search_history", "Results text", [img])
        };

        provider.AppendToolResults(transcript, results);

        // 1 tool result message + 1 user image message
        Assert.Equal(2, transcript.Count);

        var toolMsg = transcript[0]!.AsObject();
        Assert.Equal("tool", toolMsg["role"]?.GetValue<string>());
        Assert.Equal("call_openai_1", toolMsg["tool_call_id"]?.GetValue<string>());
        Assert.Equal("Results text", toolMsg["content"]?.GetValue<string>());

        var userMsg = transcript[1]!.AsObject();
        Assert.Equal("user", userMsg["role"]?.GetValue<string>());
        var contents = userMsg["content"]!.AsArray();
        Assert.Equal(2, contents.Count);
        Assert.Equal("text", contents[0]?["type"]?.GetValue<string>());
        Assert.Equal("Images for the search results above (redacted).", contents[0]?["text"]?.GetValue<string>());
        Assert.Equal("image_url", contents[1]?["type"]?.GetValue<string>());
        Assert.Equal($"data:image/jpeg;base64,{Convert.ToBase64String([10, 20, 30])}", contents[1]?["image_url"]?["url"]?.GetValue<string>());

        // If no images, user message is not appended
        var transcriptNoImg = new JsonArray();
        provider.AppendToolResults(transcriptNoImg, [new("call_2", "search_history", "No imgs", [])]);
        Assert.Single(transcriptNoImg);
        Assert.Equal("tool", transcriptNoImg[0]?["role"]?.GetValue<string>());
    }

    [Fact]
    public void AppendToolResults_Claude()
    {
        var provider = new ClaudeChat();
        var transcript = new JsonArray();
        var img = new OutgoingImage([11, 22, 33], "image/png", 10, 10, true);

        var results = new List<ToolOutput>
        {
            new("toolu_claude_1", "search_history", "Claude result text", [img])
        };

        provider.AppendToolResults(transcript, results);

        Assert.Single(transcript);
        var userMsg = transcript[0]!.AsObject();
        Assert.Equal("user", userMsg["role"]?.GetValue<string>());

        var contentArray = userMsg["content"]!.AsArray();
        Assert.Single(contentArray);

        var toolResult = contentArray[0]!.AsObject();
        Assert.Equal("tool_result", toolResult["type"]?.GetValue<string>());
        Assert.Equal("toolu_claude_1", toolResult["tool_use_id"]?.GetValue<string>());

        var inner = toolResult["content"]!.AsArray();
        Assert.Equal(2, inner.Count);
        Assert.Equal("text", inner[0]?["type"]?.GetValue<string>());
        Assert.Equal("Claude result text", inner[0]?["text"]?.GetValue<string>());

        Assert.Equal("image", inner[1]?["type"]?.GetValue<string>());
        var src = inner[1]?["source"]?.AsObject();
        Assert.NotNull(src);
        Assert.Equal("base64", src["type"]?.GetValue<string>());
        Assert.Equal("image/png", src["media_type"]?.GetValue<string>());
        Assert.Equal(Convert.ToBase64String([11, 22, 33]), src["data"]?.GetValue<string>());
    }

    [Fact]
    public void AppendUser_And_AppendAssistant()
    {
        IChatProvider[] providers = [new GeminiChat(), new OpenAiChat(), new ClaudeChat()];
        foreach (var p in providers)
        {
            var transcript = new JsonArray();
            p.AppendUser(transcript, "user question");
            Assert.Single(transcript);
            Assert.Equal("user", transcript[0]?["role"]?.GetValue<string>());

            var dummyAssistant = new JsonObject { ["role"] = "assistant", ["content"] = "hello" };
            var turn = new ProviderTurn(dummyAssistant, "hello", [], 10, 20, "end");
            p.AppendAssistant(transcript, turn);
            Assert.Equal(2, transcript.Count);
            // Verify DeepClone: modifying original node does not affect transcript
            dummyAssistant["content"] = "modified";
            Assert.Equal("hello", transcript[1]?["content"]?.GetValue<string>());
        }
    }

    [Fact]
    public void ChatProviders_For_ReturnsCorrectInstances_And_ThrowsOnUnknown()
    {
        var gemini = ChatProviders.For("gemini");
        Assert.Equal("gemini", gemini.Name);
        Assert.IsType<GeminiChat>(gemini);
        Assert.Same(gemini, ChatProviders.For("gemini"));

        var openAi = ChatProviders.For("openai-compatible");
        Assert.Equal("openai-compatible", openAi.Name);
        Assert.IsType<OpenAiChat>(openAi);
        Assert.Same(openAi, ChatProviders.For("openai-compatible"));

        var claude = ChatProviders.For("claude");
        Assert.Equal("claude", claude.Name);
        Assert.IsType<ClaudeChat>(claude);
        Assert.Same(claude, ChatProviders.For("claude"));

        var ex = Assert.Throws<ArgumentException>(() => ChatProviders.For("mistral"));
        Assert.Contains("mistral", ex.Message);
    }

    [Fact]
    public async Task SseReader_MultiLine_Comments_TrailingEventWithoutBlankLine()
    {
        var sse =
            """
            : leading comment
            : another comment
            event: update
            data: line 1
            data: line 2

            : middle comment
            data: trailing event data
            """;

        using var stream = StreamOf(sse);
        var events = new List<(string Event, string Data)>();
        await foreach (var item in SseReader.ReadAsync(stream, CancellationToken.None))
        {
            events.Add(item);
        }

        Assert.Equal(2, events.Count);

        // Event 1: multi-line data joined with "\n", comment lines ignored
        Assert.Equal("update", events[0].Event);
        Assert.Equal("line 1\nline 2", events[0].Data);

        // Event 2: default empty event name, final event without trailing blank line still dispatched
        Assert.Equal("", events[1].Event);
        Assert.Equal("trailing event data", events[1].Data);
    }
}
