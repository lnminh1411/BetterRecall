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

internal sealed class ProcessWorkerLauncher(string workerExe, bool ecoQos) : IWorkerLauncher
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
        if (ecoQos) Governance.ApplyEcoQos(p.Handle, idlePriority: true);
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
    private readonly SemaphoreSlim _startLock = new(1, 1);
    private readonly Timer _idleTimer;
    private WorkerConnection? _conn;
    private int _pid;
    private int _failures;
    private long _nextAttemptTicks;
    private bool _disposed;

    public WorkerHello? Hello { get; private set; }
    public event Action<WorkerHello>? HelloReceived;
    public TimeSpan ConnectTimeout { get; init; } = TimeSpan.FromSeconds(30);
    public bool VerifyClientPid { get; init; } = true;

    public WorkerSupervisor(IWorkerLauncher launcher, string home, Func<int> idleExitS)
    {
        _launcher = launcher;
        _home = home;
        _idleExitS = idleExitS;
        _idleTimer = new Timer(_ => CheckIdle(), null, 5000, 5000);
    }

    public bool IsRunning => _conn is { IsClosed: false };
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

    /// <summary>Starts the worker when it is not running. Throws WorkerException while in backoff.</summary>
    public async Task<WorkerConnection> EnsureAsync(CancellationToken ct)
    {
        var c = _conn;
        if (c is { IsClosed: false }) return c;
        await _startLock.WaitAsync(ct).ConfigureAwait(false);
        try
        {
            c = _conn;
            if (c is { IsClosed: false }) return c;
            ObjectDisposedException.ThrowIf(_disposed, this);
            long wait = Interlocked.Read(ref _nextAttemptTicks) - Environment.TickCount64;
            if (wait > 0) throw new WorkerException($"Worker restart backoff ({wait} ms).");
            try
            {
                c = await StartAsync(ct).ConfigureAwait(false);
                _conn = c;
                return c;
            }
            catch (Exception ex) when (ex is not OperationCanceledException)
            {
                NoteFailure();
                Log.Warn("worker start failed", ex);
                throw ex as WorkerException ?? new WorkerException("Worker failed to start.");
            }
        }
        finally { _startLock.Release(); }
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
        int pid;
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
            var hello = await conn.SendAsync(new WorkerRequest(0, "hello"), ReadOnlyMemory<byte>.Empty, ct).ConfigureAwait(false);
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
        if (crashed && !_disposed) NoteFailure();
        else Interlocked.Exchange(ref _failures, 0);
        if (ReferenceEquals(_conn, conn)) _conn = null;
    }

    public void MarkHealthy() => Interlocked.Exchange(ref _failures, 0);

    private void CheckIdle()
    {
        var c = _conn;
        if (c is null || c.IsClosed || c.Pending > 0) return;
        // The worker exits by itself after inference.idleExitS; this is the backstop.
        if (c.IdleMs > (_idleExitS() + 15) * 1000L)
        {
            Log.Info("worker idle, sending shutdown");
            _ = ShutdownAsync();
        }
    }

    public async Task ShutdownAsync()
    {
        var c = _conn;
        if (c is null) return;
        _conn = null;
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
        await ShutdownAsync().ConfigureAwait(false);
    }
}
