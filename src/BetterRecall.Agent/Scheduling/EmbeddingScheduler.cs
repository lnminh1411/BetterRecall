// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using BetterRecall.Agent.Capture;
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
    private readonly Func<int>? _indexDim;
    private readonly Queue<PendingJob> _queue = new();
    private readonly object _qlock = new();
    private readonly SemaphoreSlim _wake = new(0, int.MaxValue);
    private readonly CancellationTokenSource _cts = new();
    private Task? _loop;
    private EmbeddingInfo? _space;
    // A17: the store instance that last had EnsureEmbeddingSpace(_space) succeed. Purge all or a late open swaps the
    // store; the new one must be told the space again or every vector misses its index.
    private IStore? _spaceStore;
    private readonly object _spaceLock = new();
    // S2/S3: worker attempts per job id (transient device errors, and crashes with the job in flight).
    private readonly Dictionary<string, int> _attempts = new();
    // A-L2: the job being served (out of _queue) and whether a purge or Clear dropped it meanwhile; under _qlock.
    private string? _inFlight;
    private bool _inFlightDropped;
    private long _dimRestartTicks = long.MinValue / 2;

    /// <summary>Transient worker errors ("Device failed:", "Internal error.") tried per job before it is settled.</summary>
    public const int MaxTransientAttempts = 5;
    /// <summary>Worker crashes with the same job in flight before it is set aside as poison.</summary>
    public const int MaxCrashAttempts = 3;

    public string LastDecision { get; private set; } = "idle";
    public int Processed { get; private set; }
    public int Dropped { get; private set; }

    /// <param name="indexDim">The agent's index.dim: a hello with another dim is refused (C5). Null skips the check
    /// (tests whose fake worker has a dim of its own).</param>
    public EmbeddingScheduler(Func<IStore?> store, Func<Settings> settings, WorkerClient worker, Func<GovernanceInputs> probe, Func<bool> paused,
        Func<int>? indexDim = null)
    {
        _store = store;
        _settings = settings;
        _worker = worker;
        _probe = probe;
        _paused = paused;
        _indexDim = indexDim;
        worker.Supervisor.HelloReceived += OnHello;
        // A worker already up (started before this scheduler) said hello before anyone listened: vectors need a space.
        if (worker.Supervisor.Hello is { } h) OnHello(h);
    }

    public int Queued { get { lock (_qlock) return _queue.Count; } }

    private void OnHello(WorkerHello h)
    {
        // A-M4: a worker without an embedder (models missing, no usable device) still says hello, with revision "".
        // It says nothing about the store's model: taking it rewrote the space, emptied the vector index and queued the
        // whole history, then again when the models came back. Keep the space; the first embed reply that names a real
        // embedder brings it (WorkerSupervisor.NoteEmbedder).
        if (string.IsNullOrEmpty(h.Model) || string.IsNullOrEmpty(h.Revision))
        {
            Log.Warn($"worker has no embedder (device {h.Device}, missing [{string.Join(",", h.Missing ?? [])}]); embedding space kept");
            return;
        }
        // C5: the dim must be the one the agent accepted (the worker gets it from the agent now). Another dim would
        // truncate every stored vector for good: keep the space and ask for a worker on the agent's settings.
        if (_indexDim?.Invoke() is int want && h.Dim != want)
        {
            Log.Warn($"worker embeds at dim {h.Dim}, not index.dim {want}; embedding space kept");
            long now = Environment.TickCount64;
            if (now - Interlocked.Read(ref _dimRestartTicks) > 600_000)
            {
                Interlocked.Exchange(ref _dimRestartTicks, now);
                _worker.Supervisor.RequestRestart("index.dim");
            }
            return;
        }
        lock (_spaceLock)
        {
            var info = new EmbeddingInfo(h.Model, h.Revision, h.Dim);
            if (_space != info) { _space = info; _spaceStore = null; }
        }
        EnsureSpace(_store());
    }

    /// <summary>Tells store the current embedding space unless it already has it. Only a success is remembered: a hello
    /// before the store opened, or a failed call, is retried on the next step (A17).</summary>
    private void EnsureSpace(IStore? store)
    {
        if (store is null) return;
        lock (_spaceLock)
        {
            if (_space is not { } info || ReferenceEquals(_spaceStore, store)) return;
            try
            {
                store.EnsureEmbeddingSpace(info);
                _spaceStore = store;
            }
            catch (Exception ex) { Log.Warn("EnsureEmbeddingSpace", ex); }
        }
    }

    /// <summary>A worker error that may pass (device lost or busy, NSFW gate failing closed, out of memory), unlike
    /// "Bad request." for input the worker can never read (S2).</summary>
    internal static bool IsTransient(WorkerReplyException ex) =>
        ex.Message.StartsWith("Device failed:", StringComparison.Ordinal) || ex.Message.StartsWith("Internal error.", StringComparison.Ordinal);

    /// <summary>The worker went away with the request in flight: it may have crashed on this very job (S3).</summary>
    internal static bool IsCrash(WorkerException ex) => ex is not WorkerReplyException && ex.Message.StartsWith("Worker pipe closed", StringComparison.Ordinal);

    /// <summary>Counts one more attempt for the job and returns the total.</summary>
    private int NoteAttempt(string id)
    {
        lock (_attempts)
        {
            int n = _attempts.GetValueOrDefault(id) + 1;
            _attempts[id] = n;
            return n;
        }
    }

    private int Attempts(string id)
    {
        lock (_attempts) return _attempts.GetValueOrDefault(id);
    }

    private void Settled(string id)
    {
        lock (_attempts) _attempts.Remove(id);
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
            if (_inFlight is not null) _inFlightDropped = true;   // not put back after its reply (A-L2)
        }
        lock (_attempts) _attempts.Clear();
    }

    /// <summary>Rows deleted from the store: their queued jobs (images in RAM) are zeroed and dropped. Returns jobs removed.</summary>
    public int Remove(IReadOnlySet<string> ids)
    {
        if (ids.Count == 0) return 0;
        lock (_qlock)
        {
            int before = _queue.Count;
            var keep = new List<PendingJob>(before);
            foreach (var j in _queue)
            {
                if (ids.Contains(j.Id)) { if (j.Image is not null) Array.Clear(j.Image); }
                else keep.Add(j);
            }
            _queue.Clear();
            foreach (var j in keep) _queue.Enqueue(j);
            // A-L2: the job being served is not in _queue: mark it, so a retryable reply does not put its image back.
            if (_inFlight is { } f && ids.Contains(f)) _inFlightDropped = true;
            return before - keep.Count;
        }
    }

    /// <summary>Tests: puts jobs in the queue as TakePendingEmbeddings would.</summary>
    internal void EnqueueForTest(IEnumerable<PendingJob> jobs)
    {
        lock (_qlock) foreach (var j in jobs) _queue.Enqueue(j);
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
        EnsureSpace(store);
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
            lock (_qlock)
            {
                if (!_queue.TryDequeue(out job)) break;
                _inFlight = job.Id;
                _inFlightDropped = false;
            }
            bool requeued = false;
            try
            {
                if (DroppedInFlight(job)) continue;   // purged between the dequeue and here
                await ProcessAsync(store, job, s, ct).ConfigureAwait(false);
                Settled(job.Id);
            }
            catch (WorkerReplyException ex) when (ex.ModelMissing)
            {
                // A model is not installed (embedder, or the NSFW detector the filter asks for, which fails closed):
                // every job fails the same way. Keep them for when the models arrive, and say so.
                requeued = Requeue(job);
                Log.Warn($"indexing waits: {ex.Message} (retry in 10 min)");
                LastDecision = "model-missing";
                return TimeSpan.FromMinutes(10);
            }
            catch (WorkerReplyException ex) when (IsTransient(ex) && NoteAttempt(job.Id) < MaxTransientAttempts)
            {
                // Device failure, a cold NPU, a TDR, out of memory: the next try may work. Keep the job (it used to be
                // settled as failed, hidden from search and its keyframe deleted) and back off (S2).
                requeued = Requeue(job);
                int n = Attempts(job.Id);
                Log.Warn($"embedding {job.Id}: {ex.Message} (attempt {n} of {MaxTransientAttempts}, will retry)");
                LastDecision = "worker-error";
                return TimeSpan.FromSeconds(Math.Min(300, 15 << Math.Min(4, n - 1)));
            }
            catch (WorkerReplyException ex)
            {
                // The worker rejected this job (unreadable image, bad text), or a transient error did not pass in
                // MaxTransientAttempts tries: settle it instead of retrying forever.
                Settled(job.Id);
                store.FailEmbedding(job.Id, ex.Message);
                Log.Debug($"embedding failed for {job.Id}: {ex.Message}");
            }
            catch (WorkerException ex) when (IsCrash(ex) && NoteAttempt(job.Id) >= MaxCrashAttempts)
            {
                // The worker died with this job in flight MaxCrashAttempts times: a poison job (an image that crashes the
                // runtime). Set it aside so the queue behind it moves again (S3).
                Settled(job.Id);
                store.FailEmbedding(job.Id, "worker crashed");
                Log.Warn($"embedding {job.Id}: the worker crashed on it {MaxCrashAttempts} times; set aside");
                LastDecision = "poison-job";
                return TimeSpan.FromSeconds(15);
            }
            catch (Exception ex) when (ex is WorkerException || (ex is OperationCanceledException && !ct.IsCancellationRequested))
            {
                // Worker unavailable, restarting, crashed (counted above), or its start timed out: keep the job (image
                // intact), try later.
                requeued = Requeue(job);
                Log.Debug($"scheduler deferred: {ex.Message}");
                LastDecision = "worker-unavailable";
                return TimeSpan.FromSeconds(15);
            }
            catch (Exception ex) when (ex is not OperationCanceledException)
            {
                // Local failure (image decode): settle it so the queue moves on.
                Settled(job.Id);
                store.FailEmbedding(job.Id, ex.GetType().Name);
                Log.Warn($"embedding job {job.Id}", ex);
            }
            finally
            {
                lock (_qlock) if (_inFlight == job.Id) _inFlight = null;
                // A re-queued job keeps its image; anything else is done with it.
                if (!requeued && job.Image is not null) Array.Clear(job.Image);
            }
        }
        return decision.NextCheck;
    }

    /// <summary>The job in flight was purged or cleared meanwhile (A-L2): settle it here; the caller zeroes its image.</summary>
    private bool DroppedInFlight(PendingJob job)
    {
        lock (_qlock) if (_inFlight != job.Id || !_inFlightDropped) return false;
        Settled(job.Id);
        Log.Debug($"embedding {job.Id}: purged while in flight, dropped");
        return true;
    }

    /// <summary>Puts the job back at the head of the queue, unless it was purged while in flight (then false).</summary>
    private bool Requeue(PendingJob job)
    {
        if (DroppedInFlight(job)) return false;
        lock (_qlock)
        {
            var rest = _queue.ToArray();
            _queue.Clear();
            _queue.Enqueue(job);
            foreach (var r in rest) _queue.Enqueue(r);
        }
        return true;
    }

    private async Task ProcessAsync(IStore store, PendingJob job, Settings s, CancellationToken ct)
    {
        if (job.Kind != CaptureKind.Audio) // screen and replay frames are images
        {
            if (job.Image is null || job.ImageMime is null) { store.FailEmbedding(job.Id, "no image"); return; }
            double? nsfw = s.Privacy.NsfwFilter ? s.Privacy.NsfwThreshold : null;
            double? detector = s.Privacy.NsfwFilter && s.Privacy.NsfwDetector ? s.Privacy.NsfwDetectorThreshold : null;
            ImageEmbedResult r;
            if (ImageCodec.CommonlyDecodable(job.ImageMime))
                r = await _worker.EmbedImageAsync(job.Image, job.ImageMime, nsfw, s.Privacy.NsfwGrid, detector, ct).ConfigureAwait(false);
            else
            {
                // AVIF / JPEG XL: the worker reads raw BGRA instead.
                var (px, w, h) = ImageCodec.DecodeBgra(job.Image, job.ImageMime);
                try { r = await _worker.EmbedBgraAsync(px, w, h, nsfw, s.Privacy.NsfwGrid, detector, ct).ConfigureAwait(false); }
                finally { Array.Clear(px); }
            }
            if (r.DroppedNsfw)
            {
                store.DropCapture(job.Id, "nsfw");
                Dropped++;
                return;
            }
            EnsureSpace(store);
            store.SetEmbedding(job.Id, r.Vector!, SpaceFor(r.Vector!.Length));
            Processed++;
        }
        else
        {
            if (string.IsNullOrEmpty(job.Text)) { store.FailEmbedding(job.Id, "no text"); return; }
            var v = await _worker.EmbedTextAsync(job.Text, "document", ct).ConfigureAwait(false);
            EnsureSpace(store);
            store.SetEmbedding(job.Id, v, SpaceFor(v.Length));
            Processed++;
        }
    }

    /// <summary>The space a vector of this length is filed under: the last hello the agent accepted. With none accepted
    /// (A-M4) or another dim (a worker whose hello was refused, C5) the vector is not stored and the job waits for a
    /// worker the agent can file under; it used to be labelled with whatever the hello said.</summary>
    private EmbeddingInfo SpaceFor(int dim)
    {
        var space = _space ?? throw new WorkerException("No embedding space accepted yet.");
        if (space.Dim != dim) throw new WorkerException($"Vector dim {dim} is not the index dim {space.Dim}.");
        return space;
    }

    public async ValueTask DisposeAsync()
    {
        _cts.Cancel();
        if (_loop is not null) { try { await _loop.ConfigureAwait(false); } catch { } }
        Clear();
        _worker.Supervisor.HelloReceived -= OnHello;
    }
}
