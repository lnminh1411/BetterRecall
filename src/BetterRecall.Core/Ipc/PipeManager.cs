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
    private IpcSettings? _publicIpc;   // permission switches the live public connections were admitted under
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
        var home = DataPaths.Home();   // the agent's own data folder: every client resolves it the same way
        return kind switch
        {
            PipeKind.Ui => PipeNames.Ui(sid, home),
            PipeKind.Mcp => PipeNames.Mcp(sid, home),
            _ => PipeNames.Public(s.Ipc.PipeName, sid, home),
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
                return allow.Length == 0 || allow.Any(a => ProcessInfo.SamePath(image, a) || SameInstallFile(_installDir, image, a));
        }
    }

    /// <summary>
    /// B12 (D6): the launcher installs every version into its own folder &lt;root&gt;\&lt;version&gt;-&lt;hash12&gt;, so an
    /// ipc.allowClients entry naming a bundled exe (the CLI) stopped matching after each update. An entry inside any
    /// version folder of the agent's install root, or the stable bin\betterrecall.cmd shim (it runs the CLI), matches
    /// the same install-relative file in the agent's own install folder. Only the agent's own files gain from this.
    /// </summary>
    internal static bool SameInstallFile(string installDir, string? image, string entry)
    {
        if (image is null || string.IsNullOrWhiteSpace(entry)) return false;
        string install, img, ent;
        try
        {
            install = Path.GetFullPath(installDir).TrimEnd('\\');
            img = Path.GetFullPath(image);
            ent = Path.GetFullPath(entry);
        }
        catch (Exception e) when (e is ArgumentException or NotSupportedException or PathTooLongException) { return false; }
        if (!img.StartsWith(install + "\\", StringComparison.OrdinalIgnoreCase) || Path.GetDirectoryName(install) is not { } root) return false;
        var rel = img[(install.Length + 1)..];
        if (!ent.StartsWith(root.TrimEnd('\\') + "\\", StringComparison.OrdinalIgnoreCase)) return false;
        var inRoot = ent[(root.TrimEnd('\\').Length + 1)..];
        int cut = inRoot.IndexOf('\\');
        if (cut <= 0) return false;
        var folder = inRoot[..cut];
        var entRel = inRoot[(cut + 1)..];
        if (string.Equals(folder, InstallLayout.BinDir, StringComparison.OrdinalIgnoreCase))
            return string.Equals(entRel, InstallLayout.CliShim, StringComparison.OrdinalIgnoreCase)
                && string.Equals(rel, InstallLayout.CliExe, StringComparison.OrdinalIgnoreCase);
        return InstallLayout.IsVersionFolder(folder) && string.Equals(entRel, rel, StringComparison.OrdinalIgnoreCase);
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

    /// <summary>count: rejections this row stands for (PipeServer merges repeats, at most one row per image a minute).</summary>
    private void OnReject(PipeKind kind, PeerInfo peer, string why, int count)
    {
        var label = kind switch { PipeKind.Ui => "ui", PipeKind.Mcp => "mcp", _ => "ipc" };
        _store()?.AppendAudit(new AuditEntry(DateTimeOffset.UtcNow, label, peer.Pid, peer.Image ?? "", "rejected", count, ""));
    }

    private async Task ApplyAsync(Settings s)
    {
        await _gate.WaitAsync().ConfigureAwait(false);
        try
        {
            if (_stopped) return;
            // Changed is raised outside the settings lock, so two racing updates can deliver their snapshots out of
            // order; the passed one may be stale and reopen a pipe the newest settings closed. Apply the newest (B13).
            s = _settings.Current;
            if (_ui == null) _ui = TryCreate(PipeKind.Ui, s);

            // The client checks run at connect only. Before any await (this runs inside the settings change when the
            // gate is free): a client removed from ipc.allowClients loses its live connection, and turning a
            // permission off drops every live public connection (subscribe streams included).
            if (_public != null && _publicIpc != null)
            {
                bool revoked = Revoked(_publicIpc, s.Ipc);
                int dropped = _public.DropWhere(peer => revoked || !ImageAllowed(PipeKind.Public, peer.Image));
                if (dropped > 0) Log.Info($"pipe Public: {dropped} connection(s) dropped after an ipc settings change");
            }
            _publicIpc = Snapshot(s.Ipc);

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

    private static IpcSettings Snapshot(IpcSettings i) => new()
    {
        AllowControl = i.AllowControl, AllowPurge = i.AllowPurge, AllowThumbnails = i.AllowThumbnails, RequireUnlocked = i.RequireUnlocked,
    };

    /// <summary>True when a permission went from on to off (or the read gate became required).</summary>
    internal static bool Revoked(IpcSettings before, IpcSettings now) =>
        (before.AllowControl && !now.AllowControl) || (before.AllowPurge && !now.AllowPurge)
        || (before.AllowThumbnails && !now.AllowThumbnails) || (!before.RequireUnlocked && now.RequireUnlocked);

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
            var tasks = new[] { _ui, _public, _mcp }.Where(p => p != null).Select(p => p!.DisposeAsync().AsTask()).ToArray();
            if (tasks.Length > 0) await Task.WhenAll(tasks).ConfigureAwait(false);
            _ui = _public = _mcp = null;
        }
        finally { _gate.Release(); }
    }

    public async ValueTask DisposeAsync() => await StopAsync().ConfigureAwait(false);
}
