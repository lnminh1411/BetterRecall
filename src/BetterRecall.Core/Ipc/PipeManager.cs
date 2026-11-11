// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using BetterRecall.Contracts;

namespace BetterRecall.Core.Ipc;

/// <summary>
/// Owns the UI pipe (always), the public pipe (ipc.enabled) and the MCP pipe (mcp.enabled), and follows
/// settings changes live: toggling a switch or renaming ipc.pipeName creates or closes the pipe and drops its connections.
/// </summary>
public sealed class PipeManager : IAsyncDisposable
{
    private readonly ISettingsStore _settings;
    private readonly CommandProcessor _processor;
    private readonly string _installDir;
    private readonly Func<IStore?> _store;
    private readonly Func<PipeKind, Settings, string> _nameFor;
    private readonly SemaphoreSlim _gate = new(1, 1);
    private PipeServer? _ui, _public, _mcp;
    private bool _stopped;
    private int _uiCount, _publicCount, _mcpCount;

    /// <summary>Total connected clients across all pipes (tray indicator).</summary>
    public event Action<int>? ClientsChanged;

    public PipeManager(ISettingsStore settings, CommandProcessor processor, string installDir, Func<IStore?> store,
        Func<PipeKind, Settings, string>? nameFor = null)
    {
        _settings = settings;
        _processor = processor;
        _installDir = installDir;
        _store = store;
        _nameFor = nameFor ?? DefaultName;
    }

    public static string DefaultName(PipeKind kind, Settings s)
    {
        var sid = PipeNames.UserSid();
        return kind switch
        {
            PipeKind.Ui => PipeNames.Ui(sid),
            PipeKind.Mcp => PipeNames.Mcp(sid),
            _ => PipeNames.Public(s.Ipc.PipeName, sid),
        };
    }

    public PipeServer? Ui => _ui;
    public PipeServer? Public => _public;
    public PipeServer? Mcp => _mcp;

    public async Task StartAsync()
    {
        _settings.Changed += OnSettingsChanged;
        await ApplyAsync(_settings.Current).ConfigureAwait(false);
    }

    private void OnSettingsChanged(Settings s) => _ = ApplyAsync(s);

    internal bool ImageAllowed(PipeKind kind, string? image)
    {
        switch (kind)
        {
            case PipeKind.Ui: return ProcessInfo.SamePath(image, InstallLayout.Full(_installDir, InstallLayout.UiExe));
            case PipeKind.Mcp: return ProcessInfo.SamePath(image, InstallLayout.Full(_installDir, InstallLayout.McpExe));
            default:
                var allow = _settings.Current.Ipc.AllowClients;
                return allow.Length == 0 || allow.Any(a => ProcessInfo.SamePath(image, a));
        }
    }

    private PipeServer Create(PipeKind kind, Settings s)
    {
        var server = new PipeServer(_nameFor(kind, s), kind, img => ImageAllowed(kind, img), _processor.ServeAsync, OnReject);
        server.ConnectedChanged += n =>
        {
            switch (kind)
            {
                case PipeKind.Ui: _uiCount = n; break;
                case PipeKind.Mcp: _mcpCount = n; break;
                default: _publicCount = n; break;
            }
            try { ClientsChanged?.Invoke(_uiCount + _publicCount + _mcpCount); } catch (Exception ex) { Log.Warn("ClientsChanged handler failed", ex); }
        };
        server.Start();
        return server;
    }

    private void OnReject(PipeKind kind, PeerInfo peer, string why)
    {
        var label = kind switch { PipeKind.Ui => "ui", PipeKind.Mcp => "mcp", _ => "ipc" };
        _store()?.AppendAudit(new AuditEntry(DateTimeOffset.UtcNow, label, peer.Pid, peer.Image ?? "", "rejected", 0, ""));
    }

    private async Task ApplyAsync(Settings s)
    {
        await _gate.WaitAsync().ConfigureAwait(false);
        try
        {
            if (_stopped) return;
            if (_ui == null) _ui = TryCreate(PipeKind.Ui, s);

            var wantPublic = s.Ipc.Enabled ? _nameFor(PipeKind.Public, s) : null;
            if (_public != null && _public.Name != (wantPublic == null ? null : PipeNames.Base(wantPublic)))
            {
                await _public.DisposeAsync().ConfigureAwait(false);
                _public = null;
            }
            if (wantPublic != null && _public == null) _public = TryCreate(PipeKind.Public, s);

            if (!s.Mcp.Enabled && _mcp != null) { await _mcp.DisposeAsync().ConfigureAwait(false); _mcp = null; }
            if (s.Mcp.Enabled && _mcp == null) _mcp = TryCreate(PipeKind.Mcp, s);
        }
        catch (Exception ex) { Log.Error("pipe settings apply failed", ex); }
        finally { _gate.Release(); }
    }

    private PipeServer? TryCreate(PipeKind kind, Settings s)
    {
        try { return Create(kind, s); }
        catch (Exception ex)
        {
            // Most likely another process holds the name (FILE_FLAG_FIRST_PIPE_INSTANCE refused it).
            Log.Error($"pipe {kind} could not be created", ex);
            return null;
        }
    }

    public async Task StopAsync()
    {
        _settings.Changed -= OnSettingsChanged;
        await _gate.WaitAsync().ConfigureAwait(false);
        try
        {
            _stopped = true;
            foreach (var p in new[] { _ui, _public, _mcp })
                if (p != null) await p.DisposeAsync().ConfigureAwait(false);
            _ui = _public = _mcp = null;
        }
        finally { _gate.Release(); }
    }

    public async ValueTask DisposeAsync() => await StopAsync().ConfigureAwait(false);
}
