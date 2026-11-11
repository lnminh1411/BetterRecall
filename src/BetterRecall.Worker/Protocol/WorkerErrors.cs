// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
namespace BetterRecall.Worker.Protocol;

/// <summary>Error strings of docs/v1-design.md section 4. The message is sent verbatim in WorkerResponse.Error.</summary>
public static class WorkerErrors
{
    public const string BadRequest = "Bad request.";
    public const string Internal = "Internal error.";
    public static string ModelNotInstalled(string name) => $"Model not installed: {name}.";
    public static string DeviceFailed(string detail) => $"Device failed: {Clean(detail)}.";

    /// <summary>One line, no trailing period, bounded length, so the wire string keeps the "Device failed: x." shape.</summary>
    internal static string Clean(string detail)
    {
        var s = (detail ?? "").Replace('\r', ' ').Replace('\n', ' ').Trim();
        while (s.EndsWith('.')) s = s[..^1].TrimEnd();
        if (s.Length > 300) s = s[..300];
        return s.Length == 0 ? "unknown" : s;
    }
}

/// <summary>Base for errors whose message goes to the agent as is.</summary>
public abstract class WorkerException(string wireMessage, Exception? inner = null) : Exception(wireMessage, inner);

public sealed class BadRequestException(string? why = null) : WorkerException(WorkerErrors.BadRequest)
{
    public string? Why { get; } = why;
}

public sealed class ModelNotInstalledException(string model) : WorkerException(WorkerErrors.ModelNotInstalled(model))
{
    public string Model { get; } = model;
}

public sealed class DeviceFailedException(string detail, Exception? inner = null) : WorkerException(WorkerErrors.DeviceFailed(detail), inner);
