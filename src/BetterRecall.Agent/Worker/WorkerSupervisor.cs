// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Diagnostics;
using System.IO.Pipes;
using System.Security.AccessControl;
using System.Security.Principal;
using BetterRecall.Agent.Native;
using BetterRecall.Agent.Util;
using BetterRecall.Contracts;

namespace BetterRecall.Agent.Worker;

/// <summary>Starts the worker process. Abstracted so tests can run a fake worker in-process.</summary>
internal interface IWorkerLauncher
{
    /// <summary>Starts a worker that connects to pipeName. Returns its process id.</summary>
    int Start(string pipeName, string home);
    void Kill(int pid);
    bool HasExited(int pid);
}

/// <param name="settingsJson">The agent's worker keys (<see cref="WorkerSettingsDiff.Json"/>), passed as --settings so the
/// worker runs on what the agent accepted, never on a settings.json the agent has not loaded (sweep 4 C5).</param>
internal sealed class ProcessWorkerLauncher(string workerExe, Func<bool> ecoQos, Func<string>? settingsJson = null) : IWorkerLauncher
{
    // Every started worker until it exits, by pid: a late kill of an old worker must still reach it after a new start.
    private readonly System.Collections.Concurrent.ConcurrentDictionary<int, Process> _processes = new();
    // Lives as long as the agent: when the agent exits or crashes, Windows closes it and kills every worker in it.
    private static readonly Lazy<KillOnCloseJob?> Job = new(() =>
    {
        var j = KillOnCloseJob.TryCreate();
        if (j is null) Log.Warn("worker job object unavailable: a worker could outlive the agent");
        return j;
    });

    public int Start(string pipeName, string home)
    {
        if (!File.Exists(workerExe)) throw new WorkerException($"Worker not found: {Path.GetFileName(workerExe)}.");
        if (!VcRuntime.Present) throw new WorkerException(VcRuntime.MissingMessage);   // T2: say why instead of a silent loop
        var psi = new ProcessStartInfo(workerExe)
        {
            UseShellExecute = false,
            CreateNoWindow = true,
            WorkingDirectory = Path.GetDirectoryName(workerExe)!,
        };
        psi.ArgumentList.Add("--pipe");
        psi.ArgumentList.Add(pipeName);
        psi.ArgumentList.Add("--home");
        psi.ArgumentList.Add(home);
        if (settingsJson?.Invoke() is { } json)
        {
            psi.ArgumentList.Add("--settings");
            psi.ArgumentList.Add(json);
        }
        foreach (var (id, old) in _processes)
            try { if (old.HasExited && _processes.TryRemove(id, out _)) old.Dispose(); } catch { }
        var p = Process.Start(psi) ?? throw new WorkerException("Worker failed to start.");
        _processes[p.Id] = p;
        try { if (Job.Value is { } job && !job.Assign(p.Handle)) Log.Warn("worker not in the job object: it could outlive the agent"); }
        catch (Exception ex) { Log.Debug($"worker job: {ex.Message}"); }
        if (ecoQos()) Governance.ApplyEcoQos(p.Handle, idlePriority: true);   // read per start: governance.ecoQos applies at the next start
        return p.Id;
    }

    public void Kill(int pid)
    {
        try
        {
            if (_processes.TryGetValue(pid, out var p) && !p.HasExited) p.Kill();
        }
        catch { }
    }

    public bool HasExited(int pid)
    {
        try { return !_processes.TryGetValue(pid, out var p) || p.HasExited; }
        catch { return true; }
    }
}

/// <summary>
/// Owns the worker process: private pipe (DACL current user only, NETWORK denied), launch with --pipe/--home,
/// client PID check, hello, restart with backoff after a crash, shutdown when idle.
/// </summary>
internal sealed class WorkerSupervisor : IAsyncDisposable
{
    private readonly IWorkerLauncher _launcher;
    private readonly string _home;
    private readonly Func<int> _idleExitS;
    private readonly Lock _startGate = new();
    private readonly CancellationTokenSource _lifetime = new();
    private readonly Timer _idleTimer;
    private Task<WorkerConnection>? _start;
    private WorkerConnection? _conn;
    private int _pid;
    private int _failures;
    private long _nextAttemptTicks;
    private volatile bool _disposed;
    private string? _restartFor;

    public WorkerHello? Hello { get; private set; }
    public event Action<WorkerHello>? HelloReceived;
    public TimeSpan ConnectTimeout { get; init; } = TimeSpan.FromSeconds(30);
    public TimeSpan HelloTimeout { get; init; } = TimeSpan.FromMinutes(10);
    public bool VerifyClientPid { get; init; } = true;
    /// <summary>Liveness: the worker has owed its oldest request a reply for this long with no frame at all (native
    /// deadlock, stuck GenAI call), so it is hung. It is killed and restarted with backoff. The old rule counted caller
    /// timeouts, which could never reach its limit against the serial worker and missed untimed callers (scheduler,
    /// transcriber) (A5, CA-S1). A request that runs long on purpose (cold Whisper or NPU OCR compile, device re-bench)
    /// is covered by the worker's keepalive frames (every 20 s, up to 15 min per request).</summary>
    public TimeSpan UnresponsiveAfter { get; init; } = TimeSpan.FromSeconds(90);
    /// <summary>Clock (ms) the liveness check runs on. Default <see cref="BetterRecall.Agent.Worker.LivenessClock.NowMs"/>, which skips time
    /// the PC slept: TickCount64 counted a sleep as worker silence and recycled a healthy worker on resume (C8).</summary>
    public Func<long>? LivenessClock { get; init; }
    /// <summary>Workers recycled because they stopped answering.</summary>
    public int Recycled => _recycled;
    private int _recycled;

    public WorkerSupervisor(IWorkerLauncher launcher, string home, Func<int> idleExitS)
    {
        _launcher = launcher;
        _home = home;
        _idleExitS = idleExitS;
        _idleTimer = new Timer(_ => CheckIdle(), null, 5000, 5000);
    }

    public bool IsRunning => _conn is { IsClosed: false };
    /// <summary>Process id of the last started worker (0 before the first start).</summary>
    public int Pid => _pid;
    public int Failures => _failures;

    public static PipeSecurity CreatePipeSecurity()
    {
        var sec = new PipeSecurity();
        var user = WindowsIdentity.GetCurrent().User!;
        sec.SetOwner(user);
        sec.SetAccessRuleProtection(isProtected: true, preserveInheritance: false);
        sec.AddAccessRule(new PipeAccessRule(user, PipeAccessRights.FullControl, AccessControlType.Allow));
        sec.AddAccessRule(new PipeAccessRule(new SecurityIdentifier(WellKnownSidType.NetworkSid, null), PipeAccessRights.FullControl, AccessControlType.Deny));
        return sec;
    }

    /// <summary>Starts the worker when it is not running. Throws WorkerException while in backoff.
    /// The start is shared and runs under the supervisor's own lifetime with ConnectTimeout and HelloTimeout, never
    /// under a caller's token: ct only ends this caller's wait. A keyframe OCR (30 s) used to cancel a slow hello (the
    /// first-run device benchmark takes longer), which killed the worker, skipped the backoff and started over at the
    /// next keyframe, for ever (A2, D28).</summary>
    public Task<WorkerConnection> EnsureAsync(CancellationToken ct)
    {
        var c = _conn;
        if (c is { IsClosed: false }) return Task.FromResult(c);
        Task<WorkerConnection> start;
        lock (_startGate)
        {
            c = _conn;
            if (c is { IsClosed: false }) return Task.FromResult(c);
            if (_disposed) return Task.FromException<WorkerConnection>(new ObjectDisposedException(nameof(WorkerSupervisor)));
            if (_start is not { IsCompleted: false })
            {
                long wait = Interlocked.Read(ref _nextAttemptTicks) - Environment.TickCount64;
                if (wait > 0) return Task.FromException<WorkerConnection>(new WorkerException($"Worker restart backoff ({wait} ms)."));
                _start = Task.Run(StartSharedAsync);
                // Every waiter may have given up; the failure is logged and counted, so mark it observed.
                _start.ContinueWith(static t => _ = t.Exception, CancellationToken.None, TaskContinuationOptions.OnlyOnFaulted | TaskContinuationOptions.ExecuteSynchronously, TaskScheduler.Default);
            }
            start = _start;
        }
        return ct.CanBeCanceled ? start.WaitAsync(ct) : start;
    }

    private async Task<WorkerConnection> StartSharedAsync()
    {
        try
        {
            var c = await StartAsync(_lifetime.Token).ConfigureAwait(false);
            if (_disposed) { await c.DisposeAsync().ConfigureAwait(false); throw new ObjectDisposedException(nameof(WorkerSupervisor)); }
            _conn = c;
            return c;
        }
        catch (Exception ex) when (!_disposed)
        {
            // A connect or hello timeout is a failure too: without backoff each timeout started another worker.
            NoteFailure();
            Log.Warn("worker start failed", ex);
            throw ex as WorkerException ?? new WorkerException(ex is OperationCanceledException ? "Worker start timed out." : "Worker failed to start.");
        }
    }

    private void NoteFailure()
    {
        int f = Interlocked.Increment(ref _failures);
        long delay = Math.Min(60_000, 1000L << Math.Min(6, f - 1));
        Interlocked.Exchange(ref _nextAttemptTicks, Environment.TickCount64 + delay);
    }

    private async Task<WorkerConnection> StartAsync(CancellationToken ct)
    {
        var name = PipeNames.NewWorker();
        var server = NamedPipeServerStreamAcl.Create(name, PipeDirection.InOut, 1, PipeTransmissionMode.Byte,
            PipeOptions.Asynchronous | PipeOptions.FirstPipeInstance, 0, 0, CreatePipeSecurity());
        int pid = 0;
        try
        {
            var sw = Stopwatch.StartNew();
            pid = _launcher.Start(name, _home);
            _pid = pid;
            using var timeout = CancellationTokenSource.CreateLinkedTokenSource(ct);
            timeout.CancelAfter(ConnectTimeout);
            await server.WaitForConnectionAsync(timeout.Token).ConfigureAwait(false);
            if (VerifyClientPid)
            {
                if (ClientPid(server) != (uint)pid)
                {
                    _launcher.Kill(pid);
                    throw new WorkerException("Worker pipe client is not the started process.");
                }
            }
            var conn = new WorkerConnection(server, LivenessClock);
            int connPid = pid;
            conn.Closed += (err, pending) => OnClosed(conn, connPid, err, pending);
            // Hello waits for model load and, after a model change, the device benchmark (minutes on an NPU compile);
            // the cap only stops a hung worker from holding the start lock forever.
            using var helloTimeout = CancellationTokenSource.CreateLinkedTokenSource(ct);
            helloTimeout.CancelAfter(HelloTimeout);
            var hello = await conn.SendAsync(new WorkerRequest(0, "hello"), ReadOnlyMemory<byte>.Empty, helloTimeout.Token).ConfigureAwait(false);
            if (!hello.Header.Ok || hello.Header.Hello is null)
                throw new WorkerException(hello.Header.Error ?? "Worker hello failed.");
            lock (_helloGate) Hello = hello.Header.Hello;
            Log.Info($"worker up pid={pid} device={Hello.Device} ep={Hello.Ep} dim={Hello.Dim} nsfw={Hello.NsfwDevice ?? "not loaded"} in {sw.ElapsedMilliseconds} ms");
            try { HelloReceived?.Invoke(Hello); } catch (Exception ex) { Log.Warn("hello handler", ex); }
            return conn;
        }
        catch
        {
            await server.DisposeAsync().ConfigureAwait(false);
            if (pid != 0) { try { _launcher.Kill(pid); } catch (Exception ex) { Log.Debug($"worker kill: {ex.Message}"); } }   // never leave an orphan
            throw;
        }
    }

    private static unsafe uint ClientPid(NamedPipeServerStream server)
    {
        uint pid;
        return Win32.GetNamedPipeClientProcessId(server.SafePipeHandle.DangerousGetHandle(), &pid) ? pid : 0;
    }

    private void OnClosed(WorkerConnection conn, int pid, Exception? err, int pendingAtClose)
    {
        // A close with requests in flight or a broken frame is a crash; a clean close while idle is the idle exit.
        bool crashed = err is not null || pendingAtClose > 0;
        if (err is not null) Log.Warn($"worker pipe closed with error {err.GetType().Name}");
        else Log.Info("worker pipe closed");
        if (conn.Recycled) { }   // killed for not answering: the failure is already counted, keep the backoff
        else if (crashed && !_disposed && !conn.ShuttingDown) NoteFailure();
        else Interlocked.Exchange(ref _failures, 0);
        if (ReferenceEquals(_conn, conn)) _conn = null;
        // A2: a broken pipe or frame used to leave the old worker running (models loaded) next to the new one. Kill this
        // connection's own process (never _pid, which a new start may already own). Recycle, abort and shutdown kill
        // themselves.
        if (crashed && !conn.Recycled && !conn.ShuttingDown && pid != 0)
        {
            try { _launcher.Kill(pid); } catch (Exception ex) { Log.Debug($"worker kill: {ex.Message}"); }
        }
        // C10: free the pipe handle, CTS and write gate on every close, the idle exit included (it used to wait for the
        // finalizer). Disposing twice is harmless.
        _ = Task.Run(async () => { try { await conn.DisposeAsync().ConfigureAwait(false); } catch { } });
    }

    public void MarkHealthy() => Interlocked.Exchange(ref _failures, 0);

    private readonly Lock _helloGate = new();

    /// <summary>A reply named the embedder that served it (A-M3, CA-S2). A bench or a run-time replan in the running
    /// worker can switch model or device with no new hello: Hello follows (status.device), and a new model or revision
    /// is raised as a hello so the scheduler files vectors under it and checks it like one.</summary>
    internal void NoteEmbedder(WorkerResponse r)
    {
        if (r.Model is null || r.Revision is null) return;
        WorkerHello next;
        lock (_helloGate)
        {
            if (Hello is not { } h) return;
            string device = r.Device ?? h.Device;
            if (h.Model == r.Model && h.Revision == r.Revision && h.Device == device) return;
            next = h with { Model = r.Model, Revision = r.Revision, Device = device, Dim = r.Dim ?? h.Dim };
            Hello = next;
            Log.Info($"worker embedder now {next.Model} rev {next.Revision} on {next.Device} (was {h.Model} rev {h.Revision} on {h.Device})");
        }
        try { HelloReceived?.Invoke(next); } catch (Exception ex) { Log.Warn("hello handler", ex); }
    }

    /// <summary>A setting the worker reads only at start changed (S4): shut the running worker down gracefully as soon as
    /// no request is in flight; the next request starts one with the new settings. No worker running: nothing to do.</summary>
    public void RequestRestart(string why)
    {
        if (_disposed) return;
        Volatile.Write(ref _restartFor, why);
        CheckIdle();
    }

    /// <summary>A restart is waiting for the in-flight requests to finish.</summary>
    public bool RestartPending => Volatile.Read(ref _restartFor) is not null;

    /// <summary>Runs the liveness and idle checks now (the timer runs them every 5 s).</summary>
    internal void CheckNow() => CheckIdle();

    private void CheckIdle()
    {
        var c = _conn;
        if (c is null || c.IsClosed)
        {
            // Nothing running: the next start reads the new settings. A start in progress may have read the old ones,
            // so keep the request until it is up.
            lock (_startGate) if (_start is not { IsCompleted: false }) Volatile.Write(ref _restartFor, null);
            return;
        }
        if (c.Oldest is { } sv && IsUnresponsive(sv.Op, sv.Ms, (long)UnresponsiveAfter.TotalMilliseconds))
        {
            Log.Warn($"worker not answering ({sv.Op} for {sv.Ms / 1000} s, {c.Pending} waiting), restarting it");
            _ = RecycleAsync(c);
            return;
        }
        if (c.Pending > 0) return;
        if (Interlocked.Exchange(ref _restartFor, null) is { } why)
        {
            Log.Info($"worker restart: {why} changed");
            _ = ShutdownAsync();
            return;
        }
        // The worker exits by itself after inference.idleExitS; this is the backstop.
        if (c.IdleMs > (_idleExitS() + 15) * 1000L)
        {
            Log.Info("worker idle, sending shutdown");
            _ = ShutdownAsync();
        }
    }

    /// <summary>Hung: the oldest request the worker owes has waited its allowance with no frame coming back. The serial
    /// worker serves requests in order, so that is the one it is working on (or stuck before). Any frame, keepalives
    /// included, restarts the silence. Bench and hello may legitimately run for many minutes (device benchmark, NPU
    /// compile) and are never judged. Pure.</summary>
    internal static bool IsUnresponsive(string servingOp, long servingMs, long silenceMs)
        => servingOp is not ("bench" or "hello") && servingMs >= silenceMs;

    /// <summary>Kills a hung worker and counts a failure, so the next request starts a new one after the backoff.</summary>
    private async Task RecycleAsync(WorkerConnection c)
    {
        try
        {
            int pid;
            lock (_startGate)
            {
                if (!ReferenceEquals(_conn, c) || c.Recycled) return;
                // Backoff and pid before _conn is cleared: no new start can begin, so pid is still this worker's.
                NoteFailure();
                pid = _pid;
                c.Recycled = true;
                _conn = null;
            }
            Interlocked.Increment(ref _recycled);
            if (pid != 0) { try { _launcher.Kill(pid); } catch (Exception ex) { Log.Debug($"worker kill: {ex.Message}"); } }
            await c.DisposeAsync().ConfigureAwait(false);
        }
        catch (Exception ex) { Log.Warn("worker recycle", ex); }
    }

    /// <summary>Stops this worker without counting a failure (no backoff): used when a long request was cancelled
    /// and the serial worker would stay busy with it. The next request starts a fresh worker.</summary>
    public async Task AbortAsync(WorkerConnection c, string why)
    {
        try
        {
            int pid;
            lock (_startGate)
            {
                if (!ReferenceEquals(_conn, c) || c.Recycled) return;
                pid = _pid;
                c.Aborted = true;    // no request in flight is to blame (A-L1)
                c.Recycled = true;   // OnClosed then neither counts a failure nor resets the count
                _conn = null;
            }
            Log.Info($"worker stopped: {why}");
            if (pid != 0) { try { _launcher.Kill(pid); } catch (Exception ex) { Log.Debug($"worker kill: {ex.Message}"); } }
            await c.DisposeAsync().ConfigureAwait(false);
        }
        catch (Exception ex) { Log.Warn("worker abort", ex); }
    }

    public async Task ShutdownAsync()
    {
        WorkerConnection? c;
        int pid;
        lock (_startGate)
        {
            // pid with c, before _conn is cleared: read after the 2 s wait it could be a worker started since (A4).
            c = _conn;
            if (c is null) return;
            pid = _pid;
            _conn = null;
        }
        c.ShuttingDown = true;   // the shutdown request may still be pending when the worker closes the pipe
        try
        {
            using var cts = new CancellationTokenSource(2000);
            var send = c.SendAsync(new WorkerRequest(0, "shutdown"), ReadOnlyMemory<byte>.Empty, cts.Token);
            await Task.WhenAny(send, Task.Delay(2000)).ConfigureAwait(false);
        }
        catch { }
        await c.DisposeAsync().ConfigureAwait(false);
        if (pid != 0)
        {
            for (int i = 0; i < 20 && !_launcher.HasExited(pid); i++) await Task.Delay(100).ConfigureAwait(false);
            if (!_launcher.HasExited(pid)) _launcher.Kill(pid);
        }
    }

    public async ValueTask DisposeAsync()
    {
        _disposed = true;
        await _idleTimer.DisposeAsync().ConfigureAwait(false);
        _lifetime.Cancel();   // ends a start in progress; StartAsync kills its process
        Task? start;
        lock (_startGate) start = _start;
        if (start is not null) { try { await start.ConfigureAwait(false); } catch { } }
        await ShutdownAsync().ConfigureAwait(false);
    }
}
