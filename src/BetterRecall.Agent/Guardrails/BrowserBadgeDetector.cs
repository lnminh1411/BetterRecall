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
/// Also reads the address bar (first Edit control in the toolbar) for the site of a capture: host only.
/// </summary>
internal sealed unsafe class BrowserBadgeDetector : IDisposable
{
    private static readonly Guid CLSID_CUIAutomation = new("ff48dba4-60ef-4201-aa87-54103eef594e");
    private static readonly Guid IID_IUIAutomation = new("30cbe57d-d9d0-452a-ab13-7ac5ac4825ee");
    private const int UIA_NamePropertyId = 30005, UIA_ControlTypePropertyId = 30003, UIA_ToolBarControlTypeId = 50021;
    private const int UIA_EditControlTypeId = 50004, UIA_ValueValuePropertyId = 30045;
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
    private nint _editCondition;
    private readonly Dictionary<nint, (BadgeResult Result, long At)> _cache = new();
    private readonly Dictionary<nint, (string Site, long At)> _sites = new();

    public double LastCheckMs { get; private set; }

    public BrowserBadgeDetector()
    {
        _uia = Ole.Create(CLSID_CUIAutomation, IID_IUIAutomation);
        _toolbarCondition = PropertyCondition(UIA_ControlTypePropertyId, VARIANT.Int32(UIA_ToolBarControlTypeId));
        _editCondition = PropertyCondition(UIA_ControlTypePropertyId, VARIANT.Int32(UIA_EditControlTypeId));
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
        // Failed probes are cached briefly too: background browser windows are probed on every scan otherwise.
        if (_cache.TryGetValue(hwnd, out var c) && now - c.At < c.Result switch { BadgeResult.Private => 600_000, BadgeResult.CheckFailed => 5_000, _ => 30_000 })
            return c.Result;
        var sw = Stopwatch.StartNew();
        var r = Probe(hwnd);
        LastCheckMs = sw.Elapsed.TotalMilliseconds;
        if (_cache.Count > 256) _cache.Clear();
        _cache[hwnd] = (r, now);
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

    /// <summary>Host in the address bar of a Chromium window, cached for 1.5 s per window. Empty when unknown.</summary>
    public string ReadSite(nint hwnd)
    {
        long now = Environment.TickCount64;
        if (_sites.TryGetValue(hwnd, out var c) && now - c.At < 1500) return c.Site;
        var site = ProbeSite(hwnd);
        if (_sites.Count > 256) _sites.Clear();
        _sites[hwnd] = (site, now);
        return site;
    }

    private string ProbeSite(nint hwnd)
    {
        nint root = 0, toolbar = 0, edit = 0;
        VARIANT v = default;
        try
        {
            var fromHandle = (delegate* unmanaged[Stdcall]<nint, nint, nint*, int>)Ole.Slot(_uia, 6);
            if (fromHandle(_uia, hwnd, &root) < 0 || root == 0) return "";
            var findFirst = (delegate* unmanaged[Stdcall]<nint, int, nint, nint*, int>)Ole.Slot(root, 5);
            if (findFirst(root, TreeScope_Descendants, _toolbarCondition, &toolbar) < 0 || toolbar == 0) return "";
            var findInToolbar = (delegate* unmanaged[Stdcall]<nint, int, nint, nint*, int>)Ole.Slot(toolbar, 5);
            if (findInToolbar(toolbar, TreeScope_Descendants, _editCondition, &edit) < 0 || edit == 0) return "";
            var getValue = (delegate* unmanaged[Stdcall]<nint, int, VARIANT*, int>)Ole.Slot(edit, 10);   // GetCurrentPropertyValue
            if (getValue(edit, UIA_ValueValuePropertyId, &v) < 0 || v.vt != VARIANT.VT_BSTR || v.ptr == 0) return "";
            return GuardrailPolicy.SiteFromAddress(System.Runtime.InteropServices.Marshal.PtrToStringBSTR(v.ptr));
        }
        catch (Exception ex)
        {
            Log.Debug($"uia site read failed: 0x{ex.HResult:X8}");
            return "";
        }
        finally
        {
            Ole.VariantClear(&v);
            Ole.Release(edit);
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
        Ole.Release(_editCondition);
        Ole.Release(_uia);
        _badgeCondition = _toolbarCondition = _editCondition = _uia = 0;
    }
}
