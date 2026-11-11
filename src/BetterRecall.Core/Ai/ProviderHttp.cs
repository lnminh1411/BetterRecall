// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Globalization;
using System.Net;
using System.Net.Http.Headers;
using System.Net.Sockets;
using System.Text.Json;

namespace BetterRecall.Core.Ai;

/// <summary>A failed cloud request as the user sees it. Code: auth, forbidden, model, context, quota, rate, overloaded,
/// server, offline, timeout, budget, refused, bad_request. RetryAt is set when the provider said when to try again.</summary>
public sealed record ChatFailure(string Code, string Message, DateTimeOffset? RetryAt = null);

public sealed class ChatFailureException(ChatFailure failure) : Exception(failure.Message)
{
    public ChatFailure Failure { get; } = failure;
}

/// <summary>An error event inside a 200 streaming response (Claude "event: error", a Gemini or OpenAI error chunk).</summary>
public sealed class ProviderStreamException(string type, string message) : Exception(message)
{
    public string Type { get; } = type;
}

/// <summary>
/// Error mapping and retry policy for every cloud request. Retries only rate limits (429 that is not a quota or
/// billing error) and server errors (5xx, 529), at most twice, waiting what Retry-After or Gemini RetryInfo says
/// (or 1 s, then 2 s), and never longer than MaxRetryWait: a longer wait fails at once with RetryAt so the UI can say
/// when to try again. Everything else fails at once with a message that never contains the API key.
/// </summary>
public static class ProviderErrors
{
    public static readonly TimeSpan MaxRetryWait = TimeSpan.FromSeconds(20);
    public const int MaxRetries = 2;

    public static ChatFailure FromHttp(int status, string body, HttpResponseHeaders? headers, string model, DateTimeOffset now, string? apiKey)
    {
        var (type, code, gstatus, message, retryDelay, reason) = ParseBody(body);
        message = Scrub(message, apiKey);
        var retryAt = RetryAfter(headers, now) ?? (retryDelay is { } d ? now + d : null);
        string Say(string text) => message.Length > 0 ? text + " (" + message + ")" : text;
        bool ctx = Has(code, "context_length_exceeded") || Has(type, "request_too_large") || status == 413
            || message.Contains("prompt is too long", StringComparison.OrdinalIgnoreCase)
            || message.Contains("context length", StringComparison.OrdinalIgnoreCase)
            || message.Contains("context window", StringComparison.OrdinalIgnoreCase)
            || message.Contains("input context is too long", StringComparison.OrdinalIgnoreCase)
            || message.Contains("too many tokens", StringComparison.OrdinalIgnoreCase);
        bool quota = status == 402 || Has(type, "insufficient_quota") || Has(code, "insufficient_quota") || Has(type, "billing_error")
            || Has(code, "credit_balance_exhausted") || Has(code, "organization_spend_limit_exceeded") || Has(code, "project_spend_limit_exceeded")
            || Has(code, "organization_usage_limit_exceeded") || Has(reason, "enforced_spend_limit_reached")
            || (Has(gstatus, "RESOURCE_EXHAUSTED") && retryDelay is null && (message.Contains("quota", StringComparison.OrdinalIgnoreCase) || status != 429));
        if (Has(reason, "API_KEY_INVALID") || status == 401 || Has(type, "authentication_error"))
            return new("auth", Say("The provider rejected the API key. Check the key in Settings > AI."));
        if (quota)
            return new("quota", Say("Your provider quota or credit is used up. Check the plan or billing with the provider; retrying will not help."), retryAt);
        if (ctx)
            return new("context", "The conversation is too long for this model. Start a new chat or ask a shorter question.");
        if (status == 403 || Has(type, "permission_error") || Has(gstatus, "PERMISSION_DENIED"))
            return new("forbidden", Say("The provider refused access (HTTP 403). The key may lack access to this model or region."));
        if (status == 404 || Has(type, "not_found_error") || Has(code, "model_not_found") || Has(gstatus, "NOT_FOUND"))
            return new("model", Say($"Unknown model or endpoint: '{model}' (HTTP 404). Check Settings > AI."));
        if (status == 429 || Has(type, "rate_limit_error") || Has(gstatus, "RESOURCE_EXHAUSTED"))
            return new("rate", Say("The provider's rate limit was reached." + WhenText(retryAt, now)), retryAt);
        if (status == 529 || status == 503 || Has(type, "overloaded_error") || Has(gstatus, "UNAVAILABLE"))
            return new("overloaded", Say("The provider is overloaded right now." + WhenText(retryAt, now)), retryAt);
        if (status >= 500)
            return new("server", Say($"The provider had a server error (HTTP {status})." + WhenText(retryAt, now)), retryAt);
        return new("bad_request", Say($"The provider refused the request (HTTP {status})."));
    }

    /// <summary>An error event inside a stream: same mapping as the matching HTTP status.</summary>
    public static ChatFailure FromStream(ProviderStreamException ex, string model, DateTimeOffset now, string? apiKey)
    {
        int status = ex.Type switch
        {
            "overloaded_error" or "UNAVAILABLE" or "server_is_overloaded" => 529,
            "rate_limit_error" or "RESOURCE_EXHAUSTED" or "rate_limit_exceeded" => 429,
            "api_error" or "INTERNAL" or "server_error" => 500,
            "authentication_error" => 401,
            "permission_error" => 403,
            "not_found_error" => 404,
            "invalid_request_error" => 400,
            _ => 500,
        };
        var body = new System.Text.Json.Nodes.JsonObject
        {
            ["error"] = new System.Text.Json.Nodes.JsonObject { ["type"] = ex.Type, ["message"] = ex.Message },
        }.ToJsonString();
        return FromHttp(status, body, null, model, now, apiKey);
    }

    public static ChatFailure Offline(string endpoint) =>
        new("offline", $"Can't reach {HostOf(endpoint)}. Check the internet connection (or that the local server is running).");

    public static ChatFailure Timeout(TimeSpan t) =>
        new("timeout", $"The provider did not answer within {(int)t.TotalSeconds} s. Try again.");

    public static bool Retryable(ChatFailure f) => f.Code is "rate" or "overloaded" or "server";

    /// <summary>How long to wait before retry n (0-based), or null when the wait is too long or unknown-and-forbidden.</summary>
    public static TimeSpan? RetryDelay(ChatFailure f, int attempt, DateTimeOffset now)
    {
        if (!Retryable(f) || attempt >= MaxRetries) return null;
        var wait = f.RetryAt is { } at ? at - now : TimeSpan.FromSeconds(1 << attempt);
        if (wait < TimeSpan.Zero) wait = TimeSpan.Zero;
        return wait <= MaxRetryWait ? wait : null;
    }

    /// <summary>Warning text when a rate-limit header shows less than 10% of requests or tokens left; null otherwise.
    /// Reads anthropic-ratelimit-* and OpenAI x-ratelimit-* (Gemini sends none).</summary>
    public static string? LowQuotaWarning(HttpResponseHeaders headers)
    {
        foreach (var (what, remaining, limit) in new[]
        {
            ("requests", "anthropic-ratelimit-requests-remaining", "anthropic-ratelimit-requests-limit"),
            ("tokens", "anthropic-ratelimit-tokens-remaining", "anthropic-ratelimit-tokens-limit"),
            ("input tokens", "anthropic-ratelimit-input-tokens-remaining", "anthropic-ratelimit-input-tokens-limit"),
            ("output tokens", "anthropic-ratelimit-output-tokens-remaining", "anthropic-ratelimit-output-tokens-limit"),
            ("requests", "x-ratelimit-remaining-requests", "x-ratelimit-limit-requests"),
            ("tokens", "x-ratelimit-remaining-tokens", "x-ratelimit-limit-tokens"),
        })
        {
            if (Num(headers, remaining) is not { } r || Num(headers, limit) is not { } l || l <= 0) continue;
            if (r < l * 0.10)
                return $"Provider rate limit almost used up: {r:N0} of {l:N0} {what} left in the current window.";
        }
        return null;
    }

    internal static DateTimeOffset? RetryAfter(HttpResponseHeaders? h, DateTimeOffset now)
    {
        if (h?.RetryAfter is not { } ra) return null;
        if (ra.Delta is { } d) return now + d;
        if (ra.Date is { } at) return at;
        return null;
    }

    private static double? Num(HttpResponseHeaders h, string name) =>
        h.TryGetValues(name, out var v) && double.TryParse(v.FirstOrDefault(), NumberStyles.Float, CultureInfo.InvariantCulture, out var n) ? n : null;

    private static bool Has(string? v, string want) => string.Equals(v, want, StringComparison.OrdinalIgnoreCase);

    private static string WhenText(DateTimeOffset? at, DateTimeOffset now)
    {
        if (at is not { } t) return " Try again in a minute.";
        var wait = t - now;
        if (wait <= TimeSpan.FromSeconds(1)) return " Try again now.";
        if (wait < TimeSpan.FromMinutes(2)) return $" Try again in {Math.Ceiling(wait.TotalSeconds):0} s.";
        return " Try again after " + t.ToLocalTime().ToString("HH:mm", CultureInfo.CurrentCulture) + ".";
    }

    internal static string HostOf(string endpoint) => Uri.TryCreate(endpoint, UriKind.Absolute, out var u) ? u.Host : "the provider";

    internal static string Scrub(string text, string? key)
    {
        if (string.IsNullOrEmpty(text)) return "";
        if (!string.IsNullOrEmpty(key)) text = text.Replace(key, "***", StringComparison.Ordinal);
        text = string.Join(' ', text.Split((char[]?)null, StringSplitOptions.RemoveEmptyEntries));
        return text.Length > 300 ? text[..300] + "..." : text;
    }

    /// <summary>error.type / error.code / error.status / error.message, Google RetryInfo.retryDelay and ErrorInfo.reason,
    /// Anthropic error.details.error_code. Accepts an object or a one-element array (Gemini sometimes wraps).</summary>
    internal static (string? Type, string? Code, string? Status, string Message, TimeSpan? RetryDelay, string? Reason) ParseBody(string body)
    {
        try
        {
            using var doc = JsonDocument.Parse(body);
            var root = doc.RootElement;
            if (root.ValueKind == JsonValueKind.Array && root.GetArrayLength() > 0) root = root[0];
            if (root.ValueKind != JsonValueKind.Object || !root.TryGetProperty("error", out var e) || e.ValueKind != JsonValueKind.Object)
                return (null, null, null, "", null, null);
            string? S(JsonElement o, string n) => o.TryGetProperty(n, out var p) && p.ValueKind == JsonValueKind.String ? p.GetString() : null;
            TimeSpan? delay = null;
            string? reason = null;
            if (e.TryGetProperty("details", out var det))
            {
                if (det.ValueKind == JsonValueKind.Array)
                    foreach (var d in det.EnumerateArray())
                    {
                        if (d.ValueKind != JsonValueKind.Object) continue;
                        if (S(d, "retryDelay") is { } rd && rd.EndsWith('s') && double.TryParse(rd[..^1], NumberStyles.Float, CultureInfo.InvariantCulture, out var sec))
                            delay = TimeSpan.FromSeconds(sec);
                        reason ??= S(d, "reason");
                    }
                else if (det.ValueKind == JsonValueKind.Object) reason = S(det, "error_code");
            }
            string? code = e.TryGetProperty("code", out var c) ? c.ValueKind == JsonValueKind.String ? c.GetString() : null : null;
            return (S(e, "type"), code, S(e, "status"), S(e, "message") ?? "", delay, reason);
        }
        catch (JsonException) { return (null, null, null, "", null, null); }
    }
}

/// <summary>Sends one cloud request with the retry policy above. Only the response headers are awaited; the caller
/// reads (streams) the body. No redirects, so the key never reaches another host.</summary>
public sealed class ProviderHttp(HttpMessageHandler? handler = null, Func<DateTimeOffset>? now = null, Func<TimeSpan, CancellationToken, Task>? delay = null)
{
    public static readonly TimeSpan HeaderTimeout = TimeSpan.FromSeconds(90);
    private readonly HttpClient _http = new(handler ?? new SocketsHttpHandler { AllowAutoRedirect = false, PooledConnectionLifetime = TimeSpan.FromMinutes(2) })
    {
        Timeout = Timeout.InfiniteTimeSpan,   // per request below; streams may run longer than the header wait
    };
    private readonly Func<DateTimeOffset> _now = now ?? (() => DateTimeOffset.UtcNow);
    private readonly Func<TimeSpan, CancellationToken, Task> _delay = delay ?? Task.Delay;

    public DateTimeOffset Now => _now();

    /// <summary>Returns a successful response (dispose it). Throws ChatFailureException on any failure.
    /// onRetry gets the failure before each wait, for a "retrying" notice.</summary>
    public async Task<HttpResponseMessage> SendAsync(Func<HttpRequestMessage> make, string endpoint, string model, string? apiKey,
        Action<ChatFailure, TimeSpan>? onRetry, CancellationToken ct)
    {
        for (int attempt = 0; ; attempt++)
        {
            ChatFailure failure;
            using (var req = make())
            using (var to = CancellationTokenSource.CreateLinkedTokenSource(ct))
            {
                to.CancelAfter(HeaderTimeout);
                HttpResponseMessage resp;
                try { resp = await _http.SendAsync(req, HttpCompletionOption.ResponseHeadersRead, to.Token).ConfigureAwait(false); }
                catch (OperationCanceledException) when (!ct.IsCancellationRequested) { throw new ChatFailureException(ProviderErrors.Timeout(HeaderTimeout)); }
                catch (HttpRequestException ex) when (ex.InnerException is SocketException or IOException || ex.HttpRequestError is HttpRequestError.NameResolutionError or HttpRequestError.ConnectionError or HttpRequestError.SecureConnectionError)
                {
                    throw new ChatFailureException(ProviderErrors.Offline(endpoint));
                }
                catch (HttpRequestException) { throw new ChatFailureException(ProviderErrors.Offline(endpoint)); }
                if (resp.IsSuccessStatusCode) return resp;
                using (resp)
                {
                    string body;
                    try { body = await resp.Content.ReadAsStringAsync(to.Token).ConfigureAwait(false); }
                    catch (Exception ex) when (ex is IOException or HttpRequestException or OperationCanceledException && !ct.IsCancellationRequested) { body = ""; }
                    failure = ProviderErrors.FromHttp((int)resp.StatusCode, body, resp.Headers, model, _now(), apiKey);
                    Log.Info($"cloud request: HTTP {(int)resp.StatusCode} -> {failure.Code} (attempt {attempt + 1})");
                }
            }
            if (ProviderErrors.RetryDelay(failure, attempt, _now()) is not { } wait) throw new ChatFailureException(failure);
            onRetry?.Invoke(failure, wait);
            await _delay(wait, ct).ConfigureAwait(false);
        }
    }

    internal static bool IsLoopback(string endpoint) =>
        Uri.TryCreate(endpoint, UriKind.Absolute, out var u) && (u.IsLoopback || u.Host.Equals("localhost", StringComparison.OrdinalIgnoreCase));
}

/// <summary>Local daily budget (ai.dailyRequests, ai.dailyTokens; 0 = no limit), counted per local day in the store.</summary>
public static class AiBudget
{
    public static string Today(DateTimeOffset now) => now.ToLocalTime().ToString("yyyy-MM-dd", CultureInfo.InvariantCulture);

    public static Contracts.AiUsage Usage(Contracts.IStore store, Contracts.AiSettings ai, DateTimeOffset now)
    {
        var day = Today(now);
        var (r, t) = store.AiUsage(day);
        return new Contracts.AiUsage(day, r, t, ai.DailyRequests, ai.DailyTokens);
    }

    /// <summary>Null when one more request is allowed, else the stop message.</summary>
    public static ChatFailure? Check(Contracts.AiUsage u)
    {
        if (u.RequestLimit > 0 && u.Requests >= u.RequestLimit)
            return new("budget", $"Today's cloud budget is used up ({u.Requests:N0} of {u.RequestLimit:N0} requests). It resets at midnight, or raise the limit in Settings > AI.");
        if (u.TokenLimit > 0 && u.Tokens >= u.TokenLimit)
            return new("budget", $"Today's cloud budget is used up ({u.Tokens:N0} of {u.TokenLimit:N0} tokens). It resets at midnight, or raise the limit in Settings > AI.");
        return null;
    }

    /// <summary>Warning once usage reaches 80% of either limit; null below.</summary>
    public static string? Warning(Contracts.AiUsage u)
    {
        if (u.RequestLimit > 0 && u.Requests >= u.RequestLimit * 0.8)
            return $"{u.Requests:N0} of today's {u.RequestLimit:N0} cloud requests used.";
        if (u.TokenLimit > 0 && u.Tokens >= u.TokenLimit * 0.8)
            return $"{u.Tokens:N0} of today's {u.TokenLimit:N0} cloud tokens used.";
        return null;
    }
}
