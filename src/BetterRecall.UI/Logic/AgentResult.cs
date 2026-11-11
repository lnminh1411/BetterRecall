// SPDX-License-Identifier: MIT
// Copyright (c) BetterRecall contributors
using System.Text.Json;

namespace BetterRecall.UI.Logic;

/// <summary>Why a request failed, as far as the UI needs to tell cases apart.</summary>
public enum AgentErrorKind
{
    None,
    /// <summary>No pipe, connect timeout, or the pipe broke.</summary>
    NotRunning,
    /// <summary>Server process path or session did not match.</summary>
    NotTrusted,
    /// <summary>Server answered "Store is locked.".</summary>
    Locked,
    /// <summary>Server answered ok:false with another message.</summary>
    Server,
    /// <summary>Response line was not valid JSON or had the wrong shape.</summary>
    Protocol,
}

/// <summary>Known error strings from docs/v1-design.md section 5 and research.md 7.6.</summary>
public static class AgentErrors
{
    public const string StoreLocked = "Store is locked.";
    public const string UnlockCancelled = "Unlock cancelled.";
    public const string WrongPassphrase = "Wrong passphrase.";
    public const string UnlockRequired = "Unlock required.";
    public const string NotFound = "Not found.";
    public const string InvalidToken = "Invalid or expired token.";
    public const string Internal = "Internal error.";
    public const string ClientNotAllowed = "Client not allowed.";
    public const string InvalidJson = "Invalid JSON.";
    public const string WrongPassword = "Wrong password.";
    public const string NoPassword = "No unlock password is set.";
    public const string PasswordRule = "Password must be 6 to 256 characters.";
    public const string PasswordInUse = "Password in use (config security.unlockMethod).";
    public const string AiOff = "Cloud describe is off (config ai.enabled).";
    public const string NoApiKey = "API key not set.";
    public const string ApiKeyRule = "Invalid API key.";
    public const string NoImage = "No image to describe.";
    public const string ProviderUnreachable = "Provider unreachable.";
    public const string ProviderTimeout = "Provider timed out.";

    /// <summary>"Invalid capture.intervalMs." gives "capture.intervalMs"; anything else gives null.</summary>
    public static string? InvalidKey(string? error)
    {
        if (error is null || !error.StartsWith("Invalid ", StringComparison.Ordinal) || !error.EndsWith('.')) return null;
        var key = error["Invalid ".Length..^1];
        return key.Length == 0 || key.Contains(' ') || key == "JSON" || key == "or expired token" ? null : key;
    }
}

/// <summary>One response. Data is valid only when Ok.</summary>
public readonly record struct AgentResult(bool Ok, JsonElement Data, AgentErrorKind Kind, string? Error)
{
    public static AgentResult Success(JsonElement data) => new(true, data, AgentErrorKind.None, null);
    public static AgentResult Fail(AgentErrorKind kind, string? error = null) => new(false, default, kind, error);

    public bool IsLocked => Kind == AgentErrorKind.Locked;

    /// <summary>Maps a parsed envelope to a result, classifying known errors.</summary>
    public static AgentResult FromEnvelope(bool ok, JsonElement data, string? error)
    {
        if (ok) return Success(data);
        if (error == AgentErrors.StoreLocked) return Fail(AgentErrorKind.Locked, error);
        return Fail(AgentErrorKind.Server, error ?? AgentErrors.Internal);
    }

    public AgentResult<T> As<T>(Func<JsonElement, T> map)
    {
        if (!Ok) return new AgentResult<T>(false, default, Kind, Error);
        try { return new AgentResult<T>(true, map(Data), AgentErrorKind.None, null); }
        catch (Exception ex) when (ex is JsonException or InvalidOperationException or KeyNotFoundException or FormatException or NotSupportedException)
        {
            return new AgentResult<T>(false, default, AgentErrorKind.Protocol, "Bad response.");
        }
    }
}

public readonly record struct AgentResult<T>(bool Ok, T? Value, AgentErrorKind Kind, string? Error)
{
    public bool IsLocked => Kind == AgentErrorKind.Locked;
}
