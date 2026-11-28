// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Diagnostics;
using System.Globalization;
using BetterRecall.Contracts;

namespace BetterRecall.UI.Logic;

/// <summary>Pure formatting, disk-space and speed helpers for the Models page (R7, stage I4).</summary>
public static class ModelsFormat
{
    public const double BytesPerMb = 1_000_000.0;
    public const double BytesPerGb = 1_000_000_000.0;

    /// <summary>Formats a byte count as GB or MB with one decimal place.</summary>
    public static string FormatSize(long bytes, CultureInfo? culture = null)
    {
        culture ??= CultureInfo.CurrentCulture;
        long clamped = Math.Max(0, bytes);
        if (clamped >= 1_000_000_000L)
            return $"{(clamped / BytesPerGb).ToString("0.0", culture)} GB";
        return $"{(clamped / BytesPerMb).ToString("0.0", culture)} MB";
    }

    /// <summary>Download size plus 5% headroom (matching `br-fetch`).</summary>
    public static long RequiredFreeBytes(long downloadBytes)
    {
        long clamped = Math.Max(0, downloadBytes);
        return clamped + (clamped + 19) / 20;
    }

    /// <summary>True when free space covers the download size plus 5%.</summary>
    public static bool HasEnoughSpace(long downloadBytes, long freeBytes) =>
        freeBytes >= RequiredFreeBytes(downloadBytes);

    /// <summary>Additional bytes required when free space is below size + 5%, else 0.</summary>
    public static long MissingSpaceBytes(long downloadBytes, long freeBytes) =>
        Math.Max(0, RequiredFreeBytes(downloadBytes) - Math.Max(0, freeBytes));

    /// <summary>First 7 characters of a commit SHA.</summary>
    public static string ShortRevision(string revision) =>
        string.IsNullOrEmpty(revision) ? "" : (revision.Length > 7 ? revision[..7] : revision);

    /// <summary>Formats transfer speed as "z MB/s" with one decimal place.</summary>
    public static string FormatSpeed(double bytesPerSecond, CultureInfo? culture = null)
    {
        culture ??= CultureInfo.CurrentCulture;
        double mb = Math.Max(0.0, bytesPerSecond) / BytesPerMb;
        return $"{mb.ToString("0.0", culture)} MB/s";
    }

    /// <summary>Formats progress as "x of y GB, z MB/s" (or MB when total is under 1 GB).</summary>
    public static string FormatProgress(long doneBytes, long totalBytes, double bytesPerSecond, CultureInfo? culture = null)
    {
        culture ??= CultureInfo.CurrentCulture;
        long done = Math.Max(0, doneBytes);
        long total = Math.Max(0, totalBytes);
        string speed = FormatSpeed(bytesPerSecond, culture);
        if (total >= 1_000_000_000L)
        {
            string doneStr = (done / BytesPerGb).ToString("0.0", culture);
            string totalStr = (total / BytesPerGb).ToString("0.0", culture);
            return $"{doneStr} of {totalStr} GB, {speed}";
        }
        string doneMb = (done / BytesPerMb).ToString("0.0", culture);
        string totalMb = (total / BytesPerMb).ToString("0.0", culture);
        return $"{doneMb} of {totalMb} MB, {speed}";
    }

    /// <summary>Resolves the expected download size for "full", "minimal", or a component id.</summary>
    public static long DownloadSizeFor(ModelsList list, string target)
    {
        if (string.Equals(target, "full", StringComparison.OrdinalIgnoreCase)) return list.FullBytes;
        if (string.Equals(target, "minimal", StringComparison.OrdinalIgnoreCase)) return list.MinimalBytes;
        var comp = list.Components.FirstOrDefault(c => string.Equals(c.Id, target, StringComparison.OrdinalIgnoreCase));
        return comp?.Bytes ?? 0;
    }
}

/// <summary>
/// Page-independent service that owns an in-flight model download stream so navigating away from
/// and back to the Models page keeps progress and cancel working.
/// </summary>
public sealed class ModelsDownloadService
{
    private readonly Func<AgentClient> _createRunClient;
    private readonly Action<Action> _dispatch;
    private readonly object _gate = new();
    private CancellationTokenSource? _cts;
    private long _lastBytes;
    private long _lastSampleTicks;
    private double _smoothedBps;

    public ModelsDownloadService(Func<AgentClient> createRunClient, Action<Action>? dispatch = null)
    {
        _createRunClient = createRunClient;
        _dispatch = dispatch ?? (a => a());
    }

    public bool IsDownloading { get; private set; }
    public string? ActiveTarget { get; private set; }
    public ModelsProgress? LastProgress { get; private set; }
    public double BytesPerSecond { get; private set; }
    public ModelsProgress? FinalResult { get; private set; }
    public string? ErrorMessage { get; private set; }

    public event Action? Changed;

    public Task StartAsync(string target)
    {
        CancellationTokenSource cts;
        lock (_gate)
        {
            if (IsDownloading) return Task.CompletedTask;
            IsDownloading = true;
            ActiveTarget = target;
            LastProgress = new ModelsProgress("checking", 0, 0);
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
        return RunAsync(target, cts);
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

    private async Task RunAsync(string target, CancellationTokenSource cts)
    {
        await using var client = _createRunClient();
        try
        {
            var result = await AgentApi.ModelsDownloadStreamAsync(client, target, OnProgress, cts.Token).ConfigureAwait(false);
            lock (_gate)
            {
                if (!result.Ok && FinalResult is null)
                {
                    ErrorMessage = result.Error ?? "Download failed.";
                    FinalResult = new ModelsProgress("failed", LastProgress?.DoneBytes ?? 0, LastProgress?.TotalBytes ?? 0, Error: ErrorMessage);
                }
            }
        }
        catch (OperationCanceledException)
        {
            lock (_gate)
            {
                FinalResult ??= new ModelsProgress("cancelled", LastProgress?.DoneBytes ?? 0, LastProgress?.TotalBytes ?? 0);
            }
        }
        finally
        {
            lock (_gate)
            {
                IsDownloading = false;
                ActiveTarget = null;
                BytesPerSecond = 0;
                if (ReferenceEquals(_cts, cts)) _cts = null;
            }
            cts.Dispose();
            Notify();
        }
    }

    private void OnProgress(ModelsProgress p)
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
