// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using BetterRecall.Agent.Util;
using BetterRecall.Agent.Worker;
using BetterRecall.Contracts;

namespace BetterRecall.Agent.Scheduling;

/// <summary>
/// Drains IStore.TakePendingEmbeddings into the worker under governance (v1-design section 7).
/// Jobs taken from the store are held in RAM until they finish; panic zeroes them.
/// </summary>
internal sealed class EmbeddingScheduler : IAsyncDisposable
{
    private readonly Func<IStore?> _store;
    private readonly Func<Settings> _settings;
    private readonly WorkerClient _worker;
    private readonly Func<GovernanceInputs> _probe;
    private readonly Func<bool> _paused;
    private readonly Queue<PendingJob> _queue = new();
    private readonly object _qlock = new();
    private readonly SemaphoreSlim _wake = new(0, int.MaxValue);
    private readonly CancellationTokenSource _cts = new();
    private Task? _loop;
    private EmbeddingInfo? _space;

    public string LastDecision { get; private set; } = "idle";
    public int Processed { get; private set; }
    public int Dropped { get; private set; }

    public EmbeddingScheduler(Func<IStore?> store, Func<Settings> settings, WorkerClient worker, Func<GovernanceInputs> probe, Func<bool> paused)
    {
        _store = store;
        _settings = settings;
        _worker = worker;
        _probe = probe;
        _paused = paused;
        worker.Supervisor.HelloReceived += OnHello;
    }

    public int Queued { get { lock (_qlock) return _queue.Count; } }

    private void OnHello(WorkerHello h)
    {
        var info = new EmbeddingInfo(h.Model, h.Revision, h.Dim);
        if (_space == info) return;
        _space = info;
        try { _store()?.EnsureEmbeddingSpace(info); }
        catch (Exception ex) { Log.Warn("EnsureEmbeddingSpace", ex); }
    }

    public void Start() => _loop = Task.Run(() => RunAsync(_cts.Token));

    /// <summary>New capture stored: check soon.</summary>
    public void Signal() => _wake.Release();

    public void Clear()
    {
        lock (_qlock)
        {
            foreach (var j in _queue) if (j.Image is not null) Array.Clear(j.Image);
            _queue.Clear();
        }
    }

    private async Task RunAsync(CancellationToken ct)
    {
        while (!ct.IsCancellationRequested)
        {
            TimeSpan wait = TimeSpan.FromSeconds(10);
            try
            {
                wait = await StepAsync(ct).ConfigureAwait(false);
            }
            catch (OperationCanceledException) when (ct.IsCancellationRequested) { break; }
            catch (Exception ex) { Log.Warn("scheduler step", ex); }
            if (wait > TimeSpan.Zero)
            {
                try { await _wake.WaitAsync(wait, ct).ConfigureAwait(false); }
                catch (OperationCanceledException) { break; }
            }
        }
    }

    internal async Task<TimeSpan> StepAsync(CancellationToken ct)
    {
        var store = _store();
        if (store is null || _paused()) { LastDecision = store is null ? "no-store" : "paused"; return TimeSpan.FromSeconds(5); }
        var s = _settings();
        bool haveWork;
        lock (_qlock) haveWork = _queue.Count > 0;
        if (!haveWork)
        {
            var jobs = store.TakePendingEmbeddings(32);
            if (jobs.Count == 0) { LastDecision = "empty"; return TimeSpan.FromSeconds(30); }
            lock (_qlock) foreach (var j in jobs) _queue.Enqueue(j);
        }
        var decision = Governance.Decide(_probe(), s.Governance, s.Capture);
        LastDecision = decision.Reason;
        if (!decision.Run) return decision.NextCheck;

        for (int i = 0; i < decision.BatchSize; i++)
        {
            PendingJob? job;
            lock (_qlock) if (!_queue.TryDequeue(out job)) break;
            try
            {
                await ProcessAsync(store, job, s, ct).ConfigureAwait(false);
            }
            catch (WorkerException ex)
            {
                // Worker unavailable or busy restarting: keep the job, try later.
                lock (_qlock)
                {
                    var rest = _queue.ToArray();
                    _queue.Clear();
                    _queue.Enqueue(job);
                    foreach (var r in rest) _queue.Enqueue(r);
                }
                Log.Debug($"scheduler deferred: {ex.Message}");
                LastDecision = "worker-unavailable";
                return TimeSpan.FromSeconds(15);
            }
            finally
            {
                if (job.Image is not null) Array.Clear(job.Image);
            }
        }
        return decision.NextCheck;
    }

    private async Task ProcessAsync(IStore store, PendingJob job, Settings s, CancellationToken ct)
    {
        if (job.Kind == CaptureKind.Screen)
        {
            if (job.Image is null || job.ImageMime is null) { store.FailEmbedding(job.Id, "no image"); return; }
            double? nsfw = s.Privacy.NsfwFilter ? s.Privacy.NsfwThreshold : null;
            double? detector = s.Privacy.NsfwFilter && s.Privacy.NsfwDetector ? s.Privacy.NsfwDetectorThreshold : null;
            var r = await _worker.EmbedImageAsync(job.Image, job.ImageMime, nsfw, s.Privacy.NsfwGrid, detector, ct).ConfigureAwait(false);
            if (r.DroppedNsfw)
            {
                store.DropCapture(job.Id, "nsfw");
                Dropped++;
                return;
            }
            store.SetEmbedding(job.Id, r.Vector!, CurrentSpace(r.Vector!.Length));
            Processed++;
        }
        else
        {
            if (string.IsNullOrEmpty(job.Text)) { store.FailEmbedding(job.Id, "no text"); return; }
            var v = await _worker.EmbedTextAsync(job.Text, "document", ct).ConfigureAwait(false);
            store.SetEmbedding(job.Id, v, CurrentSpace(v.Length));
            Processed++;
        }
    }

    private EmbeddingInfo CurrentSpace(int dim)
    {
        var h = _worker.Supervisor.Hello;
        return _space ?? new EmbeddingInfo(h?.Model ?? "unknown", h?.Revision ?? "", dim);
    }

    public async ValueTask DisposeAsync()
    {
        _cts.Cancel();
        if (_loop is not null) { try { await _loop.ConfigureAwait(false); } catch { } }
        Clear();
        _worker.Supervisor.HelloReceived -= OnHello;
    }
}
