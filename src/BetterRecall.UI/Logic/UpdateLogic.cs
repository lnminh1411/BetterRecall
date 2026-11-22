// SPDX-License-Identifier: MIT
// Copyright (c) BetterRecall contributors
using System.Diagnostics;
using System.Globalization;
using BetterRecall.Contracts;

namespace BetterRecall.UI.Logic;

public enum UpdateStatusKind
{
    NotChecked,
    UpToDate,
    Available,
    Ready,
    Applying,
    Error,
}

/// <summary>Pure status selection, formatting, and URL allowlist helpers for the Update page (R8, stage I5).</summary>
public static class UpdateLogic
{
    public const string AllowedReleaseUrlPrefix = "https://github.com/lnminh1411/BetterRecall/releases/";

    /// <summary>True only when `url` is a GitHub Releases URL for this repository.</summary>
    public static bool IsAllowedNotesUrl(string? url) =>
        !string.IsNullOrWhiteSpace(url) && url.StartsWith(AllowedReleaseUrlPrefix, StringComparison.Ordinal);

    /// <summary>Formats a byte count as MB or GB with one decimal place.</summary>
    public static string FormatSize(long bytes, CultureInfo? culture = null) =>
        ModelsFormat.FormatSize(bytes, culture);

    /// <summary>Formats transfer speed as "z MB/s" with one decimal place.</summary>
    public static string FormatSpeed(double bytesPerSecond, CultureInfo? culture = null) =>
        ModelsFormat.FormatSpeed(bytesPerSecond, culture);

    /// <summary>Formats download progress as "x of y MB, z MB/s" (or GB when total is at least 1 GB).</summary>
    public static string FormatProgress(long doneBytes, long totalBytes, double bytesPerSecond, CultureInfo? culture = null) =>
        ModelsFormat.FormatProgress(doneBytes, totalBytes, bytesPerSecond, culture);

    /// <summary>Classifies an `UpdateStatus` into the headline state shown on the Update page.</summary>
    public static UpdateStatusKind Classify(UpdateStatus? status)
    {
        if (status is null) return UpdateStatusKind.NotChecked;
        if (status.State == "applying") return UpdateStatusKind.Applying;
        if (status.State == "error" && !string.IsNullOrWhiteSpace(status.Error)) return UpdateStatusKind.Error;
        if (status.Available && !string.IsNullOrWhiteSpace(status.DownloadedVersion)) return UpdateStatusKind.Ready;
        if (status.Available) return UpdateStatusKind.Available;
        if (status.LastCheck is not null) return UpdateStatusKind.UpToDate;
        return UpdateStatusKind.NotChecked;
    }

    /// <summary>True when "Download and install" may be pressed.</summary>
    public static bool CanDownload(UpdateStatus? status, bool downloading, bool applying) =>
        !downloading && !applying && status is { Available: true, CanInstall: true };

    /// <summary>True when the "Install updates automatically" toggle may be changed.</summary>
    public static bool CanToggleAuto(UpdateStatus? status) =>
        status is { CanInstall: true };
}

/// <summary>
/// Page-independent service that owns an in-flight update download stream so navigating away from
/// and back to the Update page keeps progress and cancel working.
/// </summary>
public sealed class UpdateDownloadService
{
    private readonly Func<AgentClient> _createRunClient;
    private readonly Action<Action> _dispatch;
    private readonly object _gate = new();
    private CancellationTokenSource? _cts;
    private long _lastBytes;
    private long _lastSampleTicks;
    private double _smoothedBps;

    public UpdateDownloadService(Func<AgentClient> createRunClient, Action<Action>? dispatch = null)
    {
        _createRunClient = createRunClient;
        _dispatch = dispatch ?? (a => a());
    }

    public bool IsDownloading { get; private set; }
    public UpdateProgress? LastProgress { get; private set; }
    public double BytesPerSecond { get; private set; }
    public UpdateProgress? FinalResult { get; private set; }
    public string? ErrorMessage { get; private set; }

    public event Action? Changed;

    public Task StartAsync()
    {
        CancellationTokenSource cts;
        lock (_gate)
        {
            if (IsDownloading) return Task.CompletedTask;
            IsDownloading = true;
            LastProgress = new UpdateProgress("checking", 0, 0);
            FinalResult = null;
            ErrorMessage = null;
            BytesPerSecond = 0;
            _lastBytes = 0;
            _lastSampleTicks = Stopwatch.GetTimestamp();
            _smoothedBps = 0;
            cts = new CancellationTokenSource();
            _cts = cts;
        }
        Notify();
        return RunAsync(cts);
    }

    public void Cancel()
    {
        CancellationTokenSource? cts;
        lock (_gate) { cts = _cts; }
        try { cts?.Cancel(); }
        catch (ObjectDisposedException) { }
    }

    public void ClearResult()
    {
        lock (_gate)
        {
            FinalResult = null;
            ErrorMessage = null;
        }
        Notify();
    }

    private async Task RunAsync(CancellationTokenSource cts)
    {
        await using var client = _createRunClient();
        try
        {
            var result = await AgentApi.UpdateDownloadStreamAsync(client, OnProgress, cts.Token).ConfigureAwait(false);
            lock (_gate)
            {
                if (!result.Ok && FinalResult is null)
                {
                    ErrorMessage = result.Error ?? "Download failed.";
                    FinalResult = new UpdateProgress("failed", LastProgress?.DoneBytes ?? 0, LastProgress?.TotalBytes ?? 0, Error: ErrorMessage);
                }
            }
        }
        catch (OperationCanceledException)
        {
            lock (_gate)
            {
                FinalResult ??= new UpdateProgress("cancelled", LastProgress?.DoneBytes ?? 0, LastProgress?.TotalBytes ?? 0);
            }
        }
        finally
        {
            lock (_gate)
            {
                IsDownloading = false;
                BytesPerSecond = 0;
                if (ReferenceEquals(_cts, cts)) _cts = null;
            }
            cts.Dispose();
            Notify();
        }
    }

    private void OnProgress(UpdateProgress p)
    {
        long now = Stopwatch.GetTimestamp();
        lock (_gate)
        {
            LastProgress = p;
            if (p.Phase == "downloading")
            {
                double dt = (double)(now - _lastSampleTicks) / Stopwatch.Frequency;
                if (dt >= 0.1 && p.DoneBytes >= _lastBytes)
                {
                    double instant = (p.DoneBytes - _lastBytes) / dt;
                    _smoothedBps = _smoothedBps <= 0 ? instant : (_smoothedBps * 0.6 + instant * 0.4);
                    BytesPerSecond = _smoothedBps;
                    _lastBytes = p.DoneBytes;
                    _lastSampleTicks = now;
                }
            }
            if (p.Phase is "done" or "cancelled" or "offline" or "failed")
            {
                FinalResult = p;
                ErrorMessage = p.Error;
            }
        }
        Notify();
    }

    private void Notify() => _dispatch(() => Changed?.Invoke());
}
