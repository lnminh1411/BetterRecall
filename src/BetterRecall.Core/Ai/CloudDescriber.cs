// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Diagnostics;
using System.Net;
using System.Net.Http.Headers;
using System.Text;
using System.Text.Json;
using BetterRecall.Contracts;

namespace BetterRecall.Core.Ai;

/// <summary>Image about to leave the PC. Redacted says whether a redactor changed or approved it.</summary>
public sealed record OutgoingImage(byte[] Bytes, string Mime, int Width, int Height, bool Redacted);

/// <summary>
/// The one hook cloud describe calls before a preview is shown and sent. The agent sets its redactor
/// (CoreHostImpl.DescribeRedactor: OCR + privacy.redact* on the outgoing image); without one the pass-through below is
/// used and the preview says the image is not redacted.
/// </summary>
public interface IDescribeRedactor
{
    Task<OutgoingImage> RedactAsync(string captureId, OutgoingImage image, CancellationToken ct);
    /// <summary>True only for a redactor that really blacks out sensitive areas. Chat shares images only then, and
    /// only images it returned with Redacted set; the pass-through keeps false, so chat sends no images.</summary>
    bool RedactsImages => false;
}

public sealed class PassThroughRedactor : IDescribeRedactor
{
    public Task<OutgoingImage> RedactAsync(string captureId, OutgoingImage image, CancellationToken ct) => Task.FromResult(image with { Redacted = false });
}

/// <summary>Provider failure with a short message that is safe to show (status code plus the provider's error text).</summary>
public sealed class DescribeException(string message) : Exception(message);

/// <summary>
/// Sends one image to the configured provider and returns its text. Request shapes (checked 2026-11-11):
/// Gemini generateContent (x-goog-api-key, contents/parts with inlineData), OpenAI-compatible chat completions
/// (Bearer key, image_url data URL), Claude Messages API (x-api-key, anthropic-version, base64 image block).
/// No redirects are followed, so the key never goes to another host.
/// </summary>
public sealed class CloudDescriber(HttpMessageHandler? handler = null)
{
    public const string Prompt = "Describe this screenshot in a few sentences: which app or website is shown and what the user is doing. Reply in plain text.";
    public const string ClaudeVersion = "2026-10-21";
    public const int MaxTextChars = 8000;

    private readonly HttpClient _http = new(handler ?? new SocketsHttpHandler { AllowAutoRedirect = false, PooledConnectionLifetime = TimeSpan.FromMinutes(2) })
    {
        Timeout = TimeSpan.FromSeconds(90),
    };

    public async Task<DescribeResult> DescribeAsync(AiSettings ai, string? apiKey, OutgoingImage image, CancellationToken ct)
    {
        var provider = ai.Provider;
        var model = AiDefaults.EffectiveModel(ai);
        using var req = BuildRequest(provider, AiDefaults.EffectiveEndpoint(ai), model, apiKey, image, Prompt);
        var sw = Stopwatch.StartNew();
        using var resp = await _http.SendAsync(req, HttpCompletionOption.ResponseContentRead, ct).ConfigureAwait(false);
        var body = await resp.Content.ReadAsStringAsync(ct).ConfigureAwait(false);
        Log.Info($"cloud describe: {provider} {model}, {image.Bytes.Length / 1024} KiB sent, HTTP {(int)resp.StatusCode}, {sw.ElapsedMilliseconds} ms");
        if (!resp.IsSuccessStatusCode)
        {
            var err = ErrorText(resp.StatusCode, body);
            if (!string.IsNullOrEmpty(apiKey)) err = err.Replace(apiKey, "***", StringComparison.Ordinal); // some providers echo the key
            throw new DescribeException(err);
        }
        var text = ParseText(provider, body);
        if (string.IsNullOrWhiteSpace(text)) throw new DescribeException("The provider returned no text.");
        text = text.Trim();
        if (text.Length > MaxTextChars) text = text[..MaxTextChars];
        return new DescribeResult(text, provider, model, sw.ElapsedMilliseconds);
    }

    internal static HttpRequestMessage BuildRequest(string provider, string endpoint, string model, string? apiKey, OutgoingImage image, string prompt)
    {
        var b64 = Convert.ToBase64String(image.Bytes);
        endpoint = endpoint.TrimEnd('/');
        HttpRequestMessage req;
        using var ms = new MemoryStream();
        using (var w = new Utf8JsonWriter(ms))
        {
            switch (provider)
            {
                case AiDefaults.OpenAi:
                    req = new HttpRequestMessage(HttpMethod.Post, endpoint + "/chat/completions");
                    if (!string.IsNullOrEmpty(apiKey)) req.Headers.Authorization = new AuthenticationHeaderValue("Bearer", apiKey);
                    w.WriteStartObject();
                    w.WriteString("model", model);
                    w.WriteStartArray("messages");
                    w.WriteStartObject();
                    w.WriteString("role", "user");
                    w.WriteStartArray("content");
                    w.WriteStartObject(); w.WriteString("type", "text"); w.WriteString("text", prompt); w.WriteEndObject();
                    w.WriteStartObject(); w.WriteString("type", "image_url");
                    w.WriteStartObject("image_url"); w.WriteString("url", $"data:{image.Mime};base64,{b64}"); w.WriteEndObject();
                    w.WriteEndObject();
                    w.WriteEndArray();
                    w.WriteEndObject();
                    w.WriteEndArray();
                    w.WriteEndObject();
                    break;
                case AiDefaults.Claude:
                    req = new HttpRequestMessage(HttpMethod.Post, endpoint + "/v1/messages");
                    req.Headers.TryAddWithoutValidation("x-api-key", apiKey ?? "");
                    req.Headers.TryAddWithoutValidation("anthropic-version", ClaudeVersion);
                    w.WriteStartObject();
                    w.WriteString("model", model);
                    w.WriteNumber("max_tokens", 1024);
                    w.WriteStartArray("messages");
                    w.WriteStartObject();
                    w.WriteString("role", "user");
                    w.WriteStartArray("content");
                    w.WriteStartObject(); w.WriteString("type", "image");
                    w.WriteStartObject("source"); w.WriteString("type", "base64"); w.WriteString("media_type", image.Mime); w.WriteString("data", b64); w.WriteEndObject();
                    w.WriteEndObject();
                    w.WriteStartObject(); w.WriteString("type", "text"); w.WriteString("text", prompt); w.WriteEndObject();
                    w.WriteEndArray();
                    w.WriteEndObject();
                    w.WriteEndArray();
                    w.WriteEndObject();
                    break;
                default:
                    var path = model.StartsWith("models/", StringComparison.Ordinal) ? model : "models/" + model;
                    req = new HttpRequestMessage(HttpMethod.Post, endpoint + "/v1beta/" + path + ":generateContent");
                    req.Headers.TryAddWithoutValidation("x-goog-api-key", apiKey ?? "");
                    w.WriteStartObject();
                    w.WriteStartArray("contents");
                    w.WriteStartObject();
                    w.WriteString("role", "user");
                    w.WriteStartArray("parts");
                    w.WriteStartObject(); w.WriteString("text", prompt); w.WriteEndObject();
                    w.WriteStartObject();
                    w.WriteStartObject("inlineData"); w.WriteString("mimeType", image.Mime); w.WriteString("data", b64); w.WriteEndObject();
                    w.WriteEndObject();
                    w.WriteEndArray();
                    w.WriteEndObject();
                    w.WriteEndArray();
                    w.WriteEndObject();
                    break;
            }
        }
        req.Content = new ByteArrayContent(ms.ToArray());
        req.Content.Headers.ContentType = new MediaTypeHeaderValue("application/json");
        return req;
    }

    /// <summary>Text of a success body, or null when the shape is unknown.</summary>
    internal static string? ParseText(string provider, string body)
    {
        try
        {
            using var doc = JsonDocument.Parse(body);
            var root = doc.RootElement;
            var sb = new StringBuilder();
            switch (provider)
            {
                case AiDefaults.OpenAi:
                    if (!root.TryGetProperty("choices", out var choices) || choices.ValueKind != JsonValueKind.Array || choices.GetArrayLength() == 0) return null;
                    if (!choices[0].TryGetProperty("message", out var msg) || !msg.TryGetProperty("content", out var content)) return null;
                    if (content.ValueKind == JsonValueKind.String) return content.GetString();
                    if (content.ValueKind == JsonValueKind.Array)
                        foreach (var part in content.EnumerateArray())
                            if (part.TryGetProperty("text", out var t) && t.ValueKind == JsonValueKind.String) sb.Append(t.GetString());
                    return sb.ToString();
                case AiDefaults.Claude:
                    if (!root.TryGetProperty("content", out var blocks) || blocks.ValueKind != JsonValueKind.Array) return null;
                    foreach (var block in blocks.EnumerateArray())
                        if (block.TryGetProperty("type", out var ty) && ty.GetString() == "text" && block.TryGetProperty("text", out var tx)) sb.Append(tx.GetString());
                    return sb.ToString();
                default:
                    if (!root.TryGetProperty("candidates", out var cands) || cands.ValueKind != JsonValueKind.Array || cands.GetArrayLength() == 0) return null;
                    if (!cands[0].TryGetProperty("content", out var c) || !c.TryGetProperty("parts", out var parts) || parts.ValueKind != JsonValueKind.Array) return null;
                    foreach (var part in parts.EnumerateArray())
                        if (part.TryGetProperty("text", out var pt) && pt.ValueKind == JsonValueKind.String) sb.Append(pt.GetString());
                    return sb.ToString();
            }
        }
        catch (JsonException) { return null; }
    }

    /// <summary>"Provider error (HTTP 401): message" with the provider's own message cut to 200 characters.</summary>
    internal static string ErrorText(HttpStatusCode status, string body)
    {
        string? message = null;
        try
        {
            using var doc = JsonDocument.Parse(body);
            var root = doc.RootElement;
            if (root.ValueKind == JsonValueKind.Array && root.GetArrayLength() > 0) root = root[0];
            if (root.ValueKind == JsonValueKind.Object && root.TryGetProperty("error", out var e))
            {
                if (e.ValueKind == JsonValueKind.String) message = e.GetString();
                else if (e.ValueKind == JsonValueKind.Object && e.TryGetProperty("message", out var m) && m.ValueKind == JsonValueKind.String) message = m.GetString();
            }
        }
        catch (JsonException) { }
        message = string.IsNullOrWhiteSpace(message) ? "" : ": " + new string(message.Where(ch => !char.IsControl(ch)).Take(200).ToArray());
        return $"Provider error (HTTP {(int)status}){message}";
    }
}
