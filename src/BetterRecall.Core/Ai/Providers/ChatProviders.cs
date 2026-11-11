// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using BetterRecall.Contracts;

namespace BetterRecall.Core.Ai;

public static class ChatProviders
{
    private static readonly GeminiChat s_gemini = new();
    private static readonly OpenAiChat s_openAi = new();
    private static readonly ClaudeChat s_claude = new();

    public static IChatProvider For(string provider) => provider switch
    {
        AiDefaults.Gemini => s_gemini,
        AiDefaults.OpenAi => s_openAi,
        AiDefaults.Claude => s_claude,
        _ => throw new ArgumentException($"Unknown provider: {provider}", nameof(provider)),
    };
}
