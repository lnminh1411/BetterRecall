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

    /// <summary>Called from the WinEvent hook on the message thread.</summary>
    public void OnForegroundEvent() => _wake.Set();

    public void Refresh() => _wake.Set();

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
                    var fg = WindowInspector.Foreground();
                    // Private windows on any monitor (badge results are cached per window, so this probes new windows only).
                    Func<WindowInfo, bool>? isPrivate = s.Privacy.BrowserBadgeDetection && badge is not null
                        ? w => w.Title.Length > 0 && GuardrailPolicy.IsChromiumBrowser(w.ProcessName) && badge.Check(w.Hwnd) == BadgeResult.Private
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
                    // A private browser window found through the badge is masked in the replay as well.
                    if (badgeResult == BadgeResult.Private && fg is not null && WindowInspector.TryGetBounds(fg.Hwnd, out var fr))
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
                    }
                    Volatile.Write(ref _snapshot, new GuardSnapshot(Environment.TickCount64, fg, decision, badgeResult, scan.AnyAffinity, masks, scan.PerMonitor, site, secrets));
                    if (fg?.Hwnd != lastFg)
                    {
                        lastFg = fg?.Hwnd ?? 0;
                        try { ForegroundChanged?.Invoke(fg); } catch { }
                    }
                }
                catch (Exception ex) { Log.Warn("guardrail scan", ex); }
                _wake.WaitOne(500);
            }
        }
        finally
        {
            badge?.Dispose();
            passwords?.Dispose();
            Ole.CoUninitialize();
        }
    }

    public void Dispose()
    {
        _stop = true;
        _wake.Set();
        _thread.Join(2000);
        _wake.Dispose();
    }
}
