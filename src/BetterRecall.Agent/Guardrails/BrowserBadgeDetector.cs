// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Diagnostics;
using BetterRecall.Agent.Native;
using BetterRecall.Agent.Util;

namespace BetterRecall.Agent.Guardrails;

/// <summary>
/// UI Automation lookup of the private-window badge in Chromium toolbars (Chrome "Incognito", Edge "InPrivate",
/// Brave "Private"). Raw IUIAutomation vtable calls, Native AOT safe. Best effort: names are localised and differ
/// per browser version. Results are cached per window handle. Must run on a COM-initialised (MTA) thread.
/// </summary>
internal sealed unsafe class BrowserBadgeDetector : IDisposable
{
    private static readonly Guid CLSID_CUIAutomation = new("ff48dba4-60ef-4201-aa87-54103eef594e");
    private static readonly Guid IID_IUIAutomation = new("30cbe57d-d9d0-452a-ab13-7ac5ac4825ee");
    private const int UIA_NamePropertyId = 30005, UIA_ControlTypePropertyId = 30003, UIA_ToolBarControlTypeId = 50021;
    private const int TreeScope_Descendants = 4;

    /// <summary>Accessible names of the private-mode badge. English plus common localisations.</summary>
    public static readonly string[] BadgeNames =
    [
        "Incognito", "InPrivate", "Private", "Private browsing", "Private window",
        "Inkognito", "Incógnito", "Navigation privée", "Privat", "Privado", "隐身", "シークレット", "시크릿",
    ];

    private nint _uia;
    private nint _badgeCondition;
    private nint _toolbarCondition;
    private readonly Dictionary<nint, (BadgeResult Result, long At)> _cache = new();

    public double LastCheckMs { get; private set; }

    public BrowserBadgeDetector()
    {
        _uia = Ole.Create(CLSID_CUIAutomation, IID_IUIAutomation);
        _toolbarCondition = PropertyCondition(UIA_ControlTypePropertyId, VARIANT.Int32(UIA_ToolBarControlTypeId));
        nint cond = 0;
        foreach (var name in BadgeNames)
        {
            nint c = NameCondition(name);
            if (cond == 0) { cond = c; continue; }
            nint or = OrCondition(cond, c);
            Ole.Release(cond);
            Ole.Release(c);
            cond = or;
        }
        _badgeCondition = cond;
    }

    public BadgeResult Check(nint hwnd)
    {
        long now = Environment.TickCount64;
        if (_cache.TryGetValue(hwnd, out var c) && now - c.At < (c.Result == BadgeResult.Private ? 600_000 : 30_000)) return c.Result;
        var sw = Stopwatch.StartNew();
        var r = Probe(hwnd);
        LastCheckMs = sw.Elapsed.TotalMilliseconds;
        if (_cache.Count > 256) _cache.Clear();
        if (r != BadgeResult.CheckFailed) _cache[hwnd] = (r, now);
        return r;
    }

    private BadgeResult Probe(nint hwnd)
    {
        nint root = 0, toolbar = 0, hit = 0;
        try
        {
            var fromHandle = (delegate* unmanaged[Stdcall]<nint, nint, nint*, int>)Ole.Slot(_uia, 6);
            if (fromHandle(_uia, hwnd, &root) < 0 || root == 0) return BadgeResult.CheckFailed;
            var findFirst = (delegate* unmanaged[Stdcall]<nint, int, nint, nint*, int>)Ole.Slot(root, 5);
            // Search the toolbar first so the walk does not descend into page content.
            if (findFirst(root, TreeScope_Descendants, _toolbarCondition, &toolbar) < 0) return BadgeResult.CheckFailed;
            nint scope = toolbar != 0 ? toolbar : root;
            var findInScope = (delegate* unmanaged[Stdcall]<nint, int, nint, nint*, int>)Ole.Slot(scope, 5);
            if (findInScope(scope, TreeScope_Descendants, _badgeCondition, &hit) < 0) return BadgeResult.CheckFailed;
            return hit != 0 ? BadgeResult.Private : BadgeResult.NoBadge;
        }
        catch (Exception ex)
        {
            Log.Debug($"uia badge check failed: 0x{ex.HResult:X8}");
            return BadgeResult.CheckFailed;
        }
        finally
        {
            Ole.Release(hit);
            Ole.Release(toolbar);
            Ole.Release(root);
        }
    }

    private nint PropertyCondition(int propertyId, VARIANT value)
    {
        var fn = (delegate* unmanaged[Stdcall]<nint, int, VARIANT, nint*, int>)Ole.Slot(_uia, 23);
        nint cond;
        int hr = fn(_uia, propertyId, value, &cond);
        if (hr < 0) throw new InvalidOperationException($"CreatePropertyCondition 0x{hr:X8}");
        return cond;
    }

    private nint NameCondition(string name)
    {
        nint bstr = Ole.SysAllocString(name);
        try { return PropertyCondition(UIA_NamePropertyId, new VARIANT { vt = VARIANT.VT_BSTR, ptr = bstr }); }
        finally { Ole.SysFreeString(bstr); }
    }

    private nint OrCondition(nint a, nint b)
    {
        var fn = (delegate* unmanaged[Stdcall]<nint, nint, nint, nint*, int>)Ole.Slot(_uia, 28);
        nint cond;
        int hr = fn(_uia, a, b, &cond);
        if (hr < 0) throw new InvalidOperationException($"CreateOrCondition 0x{hr:X8}");
        return cond;
    }

    public void Dispose()
    {
        Ole.Release(_badgeCondition);
        Ole.Release(_toolbarCondition);
        Ole.Release(_uia);
        _badgeCondition = _toolbarCondition = _uia = 0;
    }
}
