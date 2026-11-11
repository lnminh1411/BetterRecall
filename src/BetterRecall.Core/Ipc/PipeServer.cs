// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.ComponentModel;
using System.IO.Pipes;
using System.Runtime.InteropServices;
using BetterRecall.Contracts;
using BetterRecall.Core.Native;
using Microsoft.Win32.SafeHandles;

namespace BetterRecall.Core.Ipc;

public enum PipeKind { Ui, Public, Mcp }

/// <summary>A connected, trust-checked client.</summary>
public sealed class PipeConnection
{
    internal PipeConnection(PipeKind kind, PeerInfo peer, NamedPipeServerStream stream)
    {
        Kind = kind;
        Peer = peer;
        Stream = stream;
    }

    public PipeKind Kind { get; }
    public PeerInfo Peer { get; }
    public NamedPipeServerStream Stream { get; }
}

/// <summary>
/// Named-pipe server per research 7.3/7.4: SDDL with the user SID (deny NETWORK, medium mandatory label),
/// PIPE_REJECT_REMOTE_CLIENTS, FILE_FLAG_FIRST_PIPE_INSTANCE on the first instance, 4 instances, byte mode, overlapped.
/// Trust checks run before a byte is read; a failed check gets "Client not allowed." and is closed.
/// </summary>
public sealed class PipeServer : IAsyncDisposable
{
    public const int Instances = 4;

    private readonly string _name;
    private readonly PipeKind _kind;
    private readonly Func<string?, bool> _imageAllowed;
    private readonly Func<PipeConnection, CancellationToken, Task> _handler;
    private readonly Action<PipeKind, PeerInfo, string>? _onReject;
    private readonly CancellationTokenSource _cts = new();
    private readonly List<Task> _loops = [];
    private readonly Lock _gate = new();
    private readonly HashSet<NamedPipeServerStream> _live = [];
    private int _connected;

    public event Action<int>? ConnectedChanged;

    public PipeServer(string name, PipeKind kind, Func<string?, bool> imageAllowed,
        Func<PipeConnection, CancellationToken, Task> handler, Action<PipeKind, PeerInfo, string>? onReject = null)
    {
        _name = PipeNames.Base(name);
        _kind = kind;
        _imageAllowed = imageAllowed;
        _handler = handler;
        _onReject = onReject;
    }

    public string Name => _name;
    public PipeKind Kind => _kind;
    public int Connected => Volatile.Read(ref _connected);

    /// <summary>SDDL from research 7.3.</summary>
    public static string Sddl(string sid) => $"O:{sid}D:P(D;;GA;;;NU)(A;;GRGW;;;{sid})S:(ML;;NWNR;;;ME)";

    /// <summary>Creates the first instance (FILE_FLAG_FIRST_PIPE_INSTANCE) and starts the accept loops. Throws when the name is taken.</summary>
    public void Start()
    {
        var first = CreateInstance(first: true);
        lock (_gate)
        {
            _loops.Add(Task.Run(() => AcceptLoop(first)));
            for (int i = 1; i < Instances; i++) _loops.Add(Task.Run(() => AcceptLoop(null)));
        }
        Log.Info($"pipe {_kind} listening");
    }

    internal NamedPipeServerStream CreateInstance(bool first)
    {
        string sid = ProcessInfo.CurrentSid;
        if (!Win32.ConvertStringSecurityDescriptorToSecurityDescriptor(Sddl(sid), 1, out var sd, out _))
            throw new Win32Exception(Marshal.GetLastPInvokeError());
        try
        {
            var sa = new Win32.SECURITY_ATTRIBUTES { nLength = Marshal.SizeOf<Win32.SECURITY_ATTRIBUTES>(), lpSecurityDescriptor = sd, bInheritHandle = 0 };
            uint open = Win32.PIPE_ACCESS_DUPLEX | Win32.FILE_FLAG_OVERLAPPED | (first ? Win32.FILE_FLAG_FIRST_PIPE_INSTANCE : 0);
            uint mode = Win32.PIPE_TYPE_BYTE | Win32.PIPE_READMODE_BYTE | Win32.PIPE_WAIT | Win32.PIPE_REJECT_REMOTE_CLIENTS;
            SafePipeHandle h = Win32.CreateNamedPipe(@"\\.\pipe\" + _name, open, mode, Instances, 65536, 65536, 0, ref sa);
            if (h.IsInvalid)
            {
                int err = Marshal.GetLastPInvokeError();
                h.Dispose();
                throw new IOException($"CreateNamedPipe failed ({err}).", new Win32Exception(err));
            }
            return new NamedPipeServerStream(PipeDirection.InOut, isAsync: true, isConnected: false, h);
        }
        finally { Win32.LocalFree(sd); }
    }

    /// <summary>Reads back the security descriptor of a pipe handle as SDDL (owner, DACL, label). For tests and diagnostics.</summary>
    public static string? ReadSddl(SafeHandle pipe)
    {
        uint info = Win32.OWNER_SECURITY_INFORMATION | Win32.DACL_SECURITY_INFORMATION | Win32.LABEL_SECURITY_INFORMATION;
        if (Win32.GetSecurityInfo(pipe, Win32.SE_KERNEL_OBJECT, info, 0, 0, 0, 0, out var sd) != 0) return null;
        try
        {
            if (!Win32.ConvertSecurityDescriptorToStringSecurityDescriptor(sd, 1, info, out var str, out _)) return null;
            try { return Marshal.PtrToStringUni(str); }
            finally { Win32.LocalFree(str); }
        }
        finally { Win32.LocalFree(sd); }
    }

    private async Task AcceptLoop(NamedPipeServerStream? first)
    {
        var ct = _cts.Token;
        var next = first;
        while (!ct.IsCancellationRequested)
        {
            NamedPipeServerStream stream;
            try { stream = next ?? CreateInstance(first: false); }
            catch (Exception ex)
            {
                if (ct.IsCancellationRequested) break;
                Log.Warn($"pipe {_kind}: instance create failed", ex);
                try { await Task.Delay(1000, ct).ConfigureAwait(false); } catch { break; }
                continue;
            }
            next = null;
            lock (_gate) _live.Add(stream);
            try
            {
                await stream.WaitForConnectionAsync(ct).ConfigureAwait(false);
                await ServeAsync(stream, ct).ConfigureAwait(false);
            }
            catch (OperationCanceledException) { }
            catch (Exception ex) { Log.Warn($"pipe {_kind}: connection ended with error", ex); }
            finally
            {
                lock (_gate) _live.Remove(stream);
                // No DisconnectNamedPipe: it discards unread data, so a client would lose the last line
                // ("Client not allowed.", "Line exceeds 4096 bytes."). Closing the handle keeps it readable.
                await stream.DisposeAsync().ConfigureAwait(false);
            }
        }
    }

    private async Task ServeAsync(NamedPipeServerStream stream, CancellationToken ct)
    {
        var handle = stream.SafePipeHandle;
        Win32.GetNamedPipeClientProcessId(handle, out var pid);
        Win32.GetNamedPipeClientSessionId(handle, out var session);
        var peer = ProcessInfo.Read((int)pid, session);
        var why = Check(peer);
        if (why != null)
        {
            Log.Warn($"pipe {_kind}: client pid {pid} rejected ({why})");
            try { _onReject?.Invoke(_kind, peer, why); } catch (Exception ex) { Log.Warn("reject audit failed", ex); }
            try
            {
                using var wto = CancellationTokenSource.CreateLinkedTokenSource(ct);
                wto.CancelAfter(2000);
                await LineProtocol.WriteLineAsync(stream, Envelope.Error("Client not allowed."), wto.Token).ConfigureAwait(false);
            }
            catch { }
            return;
        }
        int n = Interlocked.Increment(ref _connected);
        RaiseConnected(n);
        try { await _handler(new PipeConnection(_kind, peer, stream), ct).ConfigureAwait(false); }
        finally { RaiseConnected(Interlocked.Decrement(ref _connected)); }
    }

    private void RaiseConnected(int n)
    {
        try { ConnectedChanged?.Invoke(n); } catch (Exception ex) { Log.Warn("ConnectedChanged handler failed", ex); }
    }

    /// <summary>Null when the peer passes research 7.4 server checks, else a short reason for the log.</summary>
    internal string? Check(PeerInfo p)
    {
        if (p.Session != ProcessInfo.CurrentSession) return "session";
        if (p.Sid == null || !string.Equals(p.Sid, ProcessInfo.CurrentSid, StringComparison.OrdinalIgnoreCase)) return "user";
        if (p.IntegrityRid < (int)Win32.SECURITY_MANDATORY_MEDIUM_RID) return "integrity";
        if (p.AppContainer) return "appcontainer";
        if (!_imageAllowed(p.Image)) return "image";
        return null;
    }

    public async Task StopAsync()
    {
        if (!_cts.IsCancellationRequested) _cts.Cancel();
        NamedPipeServerStream[] live;
        lock (_gate) live = [.. _live];
        foreach (var s in live) { try { s.Dispose(); } catch { } }
        Task[] loops;
        lock (_gate) loops = [.. _loops];
        try { await Task.WhenAll(loops).WaitAsync(TimeSpan.FromSeconds(5)).ConfigureAwait(false); }
        catch (Exception ex) { Log.Debug($"pipe {_kind} stop: {ex.GetType().Name}"); }
        Log.Info($"pipe {_kind} stopped");
    }

    public async ValueTask DisposeAsync()
    {
        await StopAsync().ConfigureAwait(false);
        _cts.Dispose();
    }
}
