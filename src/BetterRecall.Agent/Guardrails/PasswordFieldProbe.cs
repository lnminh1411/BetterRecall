// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Diagnostics;
using BetterRecall.Agent.Native;
using BetterRecall.Agent.Util;

namespace BetterRecall.Agent.Guardrails;

/// <summary>
/// UI Automation search for password fields (IsPassword) in a window, for privacy.redactPasswords: their screen
/// rectangles are blacked out in keyframes and the replay. Raw IUIAutomation vtable calls, Native AOT safe; must run
/// on a COM-initialised (MTA) thread. Results are kept per window for MinIntervalMs, since a search walks the whole
/// element tree (a browser page included).
/// </summary>
internal sealed unsafe class PasswordFieldProbe : IDisposable
{
    private static readonly Guid CLSID_CUIAutomation = new("ff48dba4-60ef-4201-aa87-54103eef594e");
    private static readonly Guid IID_IUIAutomation = new("30cbe57d-d9d0-452a-ab13-7ac5ac4825ee");
    private const int UIA_IsPasswordPropertyId = 30019;
    private const int TreeScope_Descendants = 4;
    public const int MaxFields = 32;
    public const long MinIntervalMs = 1000;

    private nint _uia;
    private nint _cond;
    private nint _lastHwnd;
    private long _lastAt = long.MinValue;
    private RECT[] _last = [];

    public double LastMs { get; private set; }

    public PasswordFieldProbe()
    {
        _uia = Ole.Create(CLSID_CUIAutomation, IID_IUIAutomation);
        var fn = (delegate* unmanaged[Stdcall]<nint, int, VARIANT, nint*, int>)Ole.Slot(_uia, 23);   // CreatePropertyCondition
        nint cond;
        int hr = fn(_uia, UIA_IsPasswordPropertyId, VARIANT.Bool(true), &cond);
        if (hr < 0) { Ole.Release(_uia); throw new InvalidOperationException($"CreatePropertyCondition 0x{hr:X8}"); }
        _cond = cond;
    }

    /// <summary>Password field rectangles (desktop pixels, non-empty) of hwnd; the cached answer when the same window
    /// was searched less than MinIntervalMs ago. Empty when the search fails.</summary>
    public RECT[] Find(nint hwnd)
    {
        long now = Environment.TickCount64;
        if (hwnd == _lastHwnd && now - _lastAt < MinIntervalMs) return _last;
        var sw = Stopwatch.StartNew();
        var r = Probe(hwnd);
        LastMs = sw.Elapsed.TotalMilliseconds;
        _lastHwnd = hwnd; _lastAt = now; _last = r;
        return r;
    }

    private RECT[] Probe(nint hwnd)
    {
        nint root = 0, array = 0;
        try
        {
            var fromHandle = (delegate* unmanaged[Stdcall]<nint, nint, nint*, int>)Ole.Slot(_uia, 6);   // ElementFromHandle
            if (fromHandle(_uia, hwnd, &root) < 0 || root == 0) return [];
            var findAll = (delegate* unmanaged[Stdcall]<nint, int, nint, nint*, int>)Ole.Slot(root, 6);   // FindAll
            if (findAll(root, TreeScope_Descendants, _cond, &array) < 0 || array == 0) return [];
            int n;
            var length = (delegate* unmanaged[Stdcall]<nint, int*, int>)Ole.Slot(array, 3);
            if (length(array, &n) < 0 || n <= 0) return [];
            var list = new List<RECT>();
            var get = (delegate* unmanaged[Stdcall]<nint, int, nint*, int>)Ole.Slot(array, 4);
            for (int i = 0; i < Math.Min(n, MaxFields); i++)
            {
                nint el = 0;
                try
                {
                    if (get(array, i, &el) < 0 || el == 0) continue;
                    RECT rc;
                    var bounds = (delegate* unmanaged[Stdcall]<nint, RECT*, int>)Ole.Slot(el, 43);   // get_CurrentBoundingRectangle
                    if (bounds(el, &rc) >= 0 && rc.Width > 0 && rc.Height > 0) list.Add(rc);
                }
                finally { Ole.Release(el); }
            }
            return list.ToArray();
        }
        catch (Exception ex)
        {
            Log.Debug($"uia password search failed: 0x{ex.HResult:X8}");
            return [];
        }
        finally
        {
            Ole.Release(array);
            Ole.Release(root);
        }
    }

    public void Dispose()
    {
        Ole.Release(_cond);
        Ole.Release(_uia);
        _cond = _uia = 0;
    }
}
