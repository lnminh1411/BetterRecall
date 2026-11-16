// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using BetterRecall.Agent.Native;
using BetterRecall.Agent.Util;
using BetterRecall.Contracts;

namespace BetterRecall.Agent.Guardrails;

/// <summary>
/// Re-evaluates the guardrails on every foreground change and twice a second, on its own MTA thread
/// (UI Automation calls can be slow). Readers take the latest snapshot; a stale snapshot counts as closed.
/// </summary>
internal sealed class GuardrailService : IDisposable
{
    private readonly Func<Settings> _settings;
    private readonly Func<bool> _sessionLocked;
    private readonly Func<IReadOnlyList<(int Index, RECT Bounds)>> _monitors;
    private readonly string _installDir;
    private readonly uint _ownPid = Win32.GetCurrentProcessId();
    private readonly AutoResetEvent _wake = new(false);
    private readonly Thread _thread;
    private volatile bool _stop;
    private GuardSnapshot _snapshot = GuardSnapshot.Closed;

    public event Action<WindowInfo?>? ForegroundChanged;
    public double LastScanMs { get; private set; }
    public double LastBadgeMs { get; private set; }
    public double LastPasswordMs { get; private set; }
    /// <summary>Password-field searches that failed (UI Automation error): those frames carry no password boxes (A15).</summary>
    public long PasswordProbeFailures;
    private long _passwordWarnAt = long.MinValue;

    public GuardrailService(Func<Settings> settings, Func<bool> sessionLocked, Func<IReadOnlyList<(int, RECT)>> monitors, string installDir)
    {
        _settings = settings;
        _sessionLocked = sessionLocked;
        _monitors = monitors;
        _installDir = installDir.TrimEnd('\\') + "\\";
        _thread = new Thread(Run) { IsBackground = true, Name = "guardrails" };
    }

    public void Start() => _thread.Start();

    /// <summary>Snapshot no older than maxAgeMs, else a closed (deny) snapshot.</summary>
    public GuardSnapshot Current(long maxAgeMs = 2000)
    {
        var s = Volatile.Read(ref _snapshot);
        return Environment.TickCount64 - s.TakenAtMs <= maxAgeMs ? s : GuardSnapshot.Closed;
    }

    /// <summary>For keyframes: Current(), but only when its scan started after the current foreground window became
    /// foreground. A scan can take a second or more (UI Automation badge probes); a keyframe taken on the previous
    /// window's "allow" would store a denied window that was alt-tabbed to meanwhile. Two cheap calls, no scan.</summary>
    public GuardSnapshot CurrentForKeyframe(long maxAgeMs = 2000)
        => FreshForGrab(Current(maxAgeMs), Interlocked.Read(ref _fgEpoch), WindowInspector.ForegroundRoot());

    /// <summary>For replay masks (A1): the bounds of the current foreground window when <paramref name="s"/> was scanned
    /// before it came up (the case keyframes refuse as ForegroundChanged), else null. The new window may be denied or a
    /// private browser window the scan has not probed yet. A Closed snapshot already masks everything. Two cheap calls.</summary>
    public RECT? UnscannedForeground(GuardSnapshot s)
    {
        if (ReferenceEquals(s, GuardSnapshot.Closed)) return null;
        nint fg = WindowInspector.ForegroundRoot();
        return UnscannedForeground(s, Interlocked.Read(ref _fgEpoch), fg, fg != 0 && WindowInspector.TryGetBounds(fg, out var r) ? r : null);
    }

    /// <summary>Pure part of <see cref="UnscannedForeground(GuardSnapshot)"/>. No foreground window now: nothing new to
    /// mask. A stale snapshot whose foreground bounds cannot be read masks everything, as no fresh snapshot does.</summary>
    internal static RECT? UnscannedForeground(GuardSnapshot s, long fgEpochNow, nint foregroundNow, RECT? foregroundBounds)
    {
        if (foregroundNow == 0 || ReferenceEquals(s, GuardSnapshot.Closed) || !ReferenceEquals(FreshForGrab(s, fgEpochNow, foregroundNow), ForegroundMoved)) return null;
        return foregroundBounds ?? Capture.CaptureService.MaskEverything;
    }

    internal static readonly GuardSnapshot ForegroundMoved = GuardSnapshot.Closed with { Decision = GuardDecision.Deny(GuardReason.ForegroundChanged) };

    /// <summary>The snapshot when no foreground event arrived since its scan started and the foreground window is still
    /// the one it scanned; else a deny snapshot. Pure.</summary>
    internal static GuardSnapshot FreshForGrab(GuardSnapshot s, long fgEpochNow, nint foregroundNow)
    {
        if (ReferenceEquals(s, GuardSnapshot.Closed)) return s;
        if (s.FgEpoch != fgEpochNow || (s.Foreground?.Hwnd ?? 0) != foregroundNow) return ForegroundMoved;
        return s;
    }

    private long _fgEpoch;

    /// <summary>Called from the WinEvent hook on the message thread.</summary>
    public void OnForegroundEvent()
    {
        Interlocked.Increment(ref _fgEpoch);
        Wake();
    }

    public void Refresh() => Wake();

    /// <summary>Session events and foreground hooks can still arrive during shutdown (A22).</summary>
    private void Wake()
    {
        if (_stop) return;
        try { _wake.Set(); } catch (ObjectDisposedException) { }
    }

    private void Run()
    {
        Ole.CoInitializeEx(0, Ole.COINIT_MULTITHREADED);
        BrowserBadgeDetector? badge = null;
        PasswordFieldProbe? passwords = null;
        nint lastFg = -1;
        try
        {
            while (!_stop)
            {
                try
                {
                    var s = _settings();
                    if ((s.Privacy.BrowserBadgeDetection || s.Privacy.RecordSite) && badge is null)
                    {
                        try { badge = new BrowserBadgeDetector(); }
                        catch (Exception ex) { Log.Warn("UI Automation unavailable", ex); }
                    }
                    var t0 = System.Diagnostics.Stopwatch.GetTimestamp();
                    long epoch = Interlocked.Read(ref _fgEpoch);   // before the foreground is read: a change during the scan makes it stale
                    var fg = WindowInspector.Foreground();
                    // Private windows on any monitor (badge results are cached per window, so this probes new windows only).
                    // A failed probe, or no UI Automation at all, hides the window too: fail closed, as for the foreground (A5).
                    Func<nint, BadgeResult>? probe = badge is null ? null : badge.Check;
                    Func<WindowInfo, bool>? isPrivate = s.Privacy.BrowserBadgeDetection
                        ? w => GuardrailPolicy.HidesBrowserWindow(w, probe)
                        : null;
                    var scan = WindowInspector.Scan(s.Privacy, _installDir, _ownPid, _monitors(), isPrivate);
                    LastScanMs = System.Diagnostics.Stopwatch.GetElapsedTime(t0).TotalMilliseconds;
                    var badgeResult = BadgeResult.NotBrowser;
                    if (fg is not null && s.Privacy.BrowserBadgeDetection && GuardrailPolicy.IsChromiumBrowser(fg.ProcessName))
                    {
                        badgeResult = badge?.Check(fg.Hwnd) ?? BadgeResult.CheckFailed;
                        LastBadgeMs = badge?.LastCheckMs ?? 0;
                    }
                    string site = "";
                    if (fg is not null && s.Privacy.RecordSite && badgeResult != BadgeResult.Private && badge is not null && GuardrailPolicy.IsChromiumBrowser(fg.ProcessName))
                        site = badge.ReadSite(fg.Hwnd);
                    var decision = GuardrailPolicy.Evaluate(fg, s.Privacy, _sessionLocked(), scan.AnyAffinity, badgeResult);
                    var masks = scan.Masks;
                    // A private browser window found through the badge, or one whose probe failed, is masked in the replay as well.
                    if (GuardrailPolicy.BadgeHides(badgeResult) && fg is not null && WindowInspector.TryGetBounds(fg.Hwnd, out var fr))
                        masks = [.. masks, fr];
                    RECT[]? secrets = null;
                    if (s.Privacy.RedactPasswords && fg is not null && decision.Allow)
                    {
                        if (passwords is null)
                        {
                            try { passwords = new PasswordFieldProbe(); }
                            catch (Exception ex) { Log.Warn("UI Automation unavailable for password fields", ex); }
                        }
                        secrets = passwords?.Find(fg.Hwnd);
                        LastPasswordMs = passwords?.LastMs ?? 0;
                        if (passwords is not null && secrets is null) NotePasswordProbeFailed(fg);
                    }
                    Volatile.Write(ref _snapshot, new GuardSnapshot(Environment.TickCount64, fg, decision, badgeResult, scan.AnyAffinity, masks, scan.PerMonitor, site, secrets, epoch));
                    if (fg?.Hwnd != lastFg)
                    {
                        lastFg = fg?.Hwnd ?? 0;
                        try { ForegroundChanged?.Invoke(fg); } catch { }
                    }
                }
                catch (Exception ex) { Log.Warn("guardrail scan", ex); }
                try { _wake.WaitOne(500); }
                catch (ObjectDisposedException) { break; }   // disposed while a scan ran past Dispose's join (A22)
            }
        }
        finally
        {
            badge?.Dispose();
            passwords?.Dispose();
            Ole.CoUninitialize();
        }
    }

    /// <summary>A failed password-field search fails open (skipping keyframes would lose every keyframe of apps whose
    /// UI Automation tree always fails), so it is counted for diagnostics and warned about at most every 10 minutes (A15).</summary>
    private void NotePasswordProbeFailed(WindowInfo fg)
    {
        Interlocked.Increment(ref PasswordProbeFailures);
        long now = Environment.TickCount64;
        if (now - _passwordWarnAt < 600_000) return;
        _passwordWarnAt = now;
        Log.Warn($"password field search failed for {fg.ProcessName} (UI Automation); its password fields are not masked ({Interlocked.Read(ref PasswordProbeFailures)} failures so far)");
    }

    public void Dispose()
    {
        _stop = true;
        try { _wake.Set(); } catch (ObjectDisposedException) { return; }
        // A scan stuck in a long UI Automation call outlives the join: leave the event to the finalizer then, so the
        // thread's next wait does not throw on a disposed handle (A22).
        bool ended = !_thread.IsAlive || _thread.Join(2000);
        if (ended) _wake.Dispose();
    }
}
