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

internal sealed class ProcessWorkerLauncher(string workerExe, Func<bool> ecoQos) : IWorkerLauncher
{
    private Process? _process;

    public int Start(string pipeName, string home)
    {
        if (!File.Exists(workerExe)) throw new WorkerException($"Worker not found: {Path.GetFileName(workerExe)}.");
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
        var p = Process.Start(psi) ?? throw new WorkerException("Worker failed to start.");
        _process = p;
        if (ecoQos()) Governance.ApplyEcoQos(p.Handle, idlePriority: true);   // read per start: governance.ecoQos applies at the next start
        return p.Id;
    }

    public void Kill(int pid)
    {
        try
        {
            if (_process is { } p && p.Id == pid && !p.HasExited) p.Kill();
        }
        catch { }
    }

    public bool HasExited(int pid)
    {
        try { return _process is not { } p || p.Id != pid || p.HasExited; }
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
    /// <summary>Liveness: this many caller timeouts in a row with no reply at all in between, and no reply for at least
    /// UnresponsiveAfter, mean the worker is hung (native deadlock, stuck GenAI call). It is killed and restarted with
    /// backoff. Without this, every new request counted as activity and a hung worker was never recycled.</summary>
    public int UnresponsiveTimeouts { get; init; } = 3;
    public TimeSpan UnresponsiveAfter { get; init; } = TimeSpan.FromSeconds(90);
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
            var conn = new WorkerConnection(server);
            conn.Closed += (err, pending) => OnClosed(conn, err, pending);
            // Hello waits for model load and, after a model change, the device benchmark (minutes on an NPU compile);
            // the cap only stops a hung worker from holding the start lock forever.
            using var helloTimeout = CancellationTokenSource.CreateLinkedTokenSource(ct);
            helloTimeout.CancelAfter(HelloTimeout);
            var hello = await conn.SendAsync(new WorkerRequest(0, "hello"), ReadOnlyMemory<byte>.Empty, helloTimeout.Token).ConfigureAwait(false);
            if (!hello.Header.Ok || hello.Header.Hello is null)
                throw new WorkerException(hello.Header.Error ?? "Worker hello failed.");
            Hello = hello.Header.Hello;
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

    private void OnClosed(WorkerConnection conn, Exception? err, int pendingAtClose)
    {
        // A close with requests in flight or a broken frame is a crash; a clean close while idle is the idle exit.
        bool crashed = err is not null || pendingAtClose > 0;
        if (err is not null) Log.Warn($"worker pipe closed with error {err.GetType().Name}");
        else Log.Info("worker pipe closed");
        if (conn.Recycled) { }   // killed for not answering: the failure is already counted, keep the backoff
        else if (crashed && !_disposed && !conn.ShuttingDown) NoteFailure();
        else Interlocked.Exchange(ref _failures, 0);
        if (ReferenceEquals(_conn, conn)) _conn = null;
    }

    public void MarkHealthy() => Interlocked.Exchange(ref _failures, 0);

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
        if (IsUnresponsive(c.ConsecutiveTimeouts, c.NoReplyMs, c.LongOpPending, UnresponsiveTimeouts, (long)UnresponsiveAfter.TotalMilliseconds))
        {
            Log.Warn($"worker not answering ({c.ConsecutiveTimeouts} timeouts in a row, no reply for {c.NoReplyMs / 1000} s), restarting it");
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

    /// <summary>Hung: callers kept timing out with nothing at all coming back, for long enough, and no long request
    /// (bench) explains the silence. Pure.</summary>
    internal static bool IsUnresponsive(int consecutiveTimeouts, long noReplyMs, bool longOpPending, int timeoutsLimit, long silenceMs)
        => !longOpPending && consecutiveTimeouts >= Math.Max(1, timeoutsLimit) && noReplyMs >= silenceMs;

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

    public async Task ShutdownAsync()
    {
        var c = _conn;
        if (c is null) return;
        _conn = null;
        c.ShuttingDown = true;   // the shutdown request may still be pending when the worker closes the pipe
        try
        {
            using var cts = new CancellationTokenSource(2000);
            var send = c.SendAsync(new WorkerRequest(0, "shutdown"), ReadOnlyMemory<byte>.Empty, cts.Token);
            await Task.WhenAny(send, Task.Delay(2000)).ConfigureAwait(false);
        }
        catch { }
        await c.DisposeAsync().ConfigureAwait(false);
        int pid = _pid;
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
