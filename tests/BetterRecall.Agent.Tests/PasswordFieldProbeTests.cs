// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Runtime.InteropServices;
using BetterRecall.Agent.Guardrails;
using BetterRecall.Agent.Native;

namespace BetterRecall.Agent.Tests;

/// <summary>UI Automation finds a password edit box in a test-owned window (a tool window placed far off screen, so
/// nothing shows on the desktop). The window lives on its own thread with a message loop, as UIA's Win32 proxy sends
/// it messages.</summary>
public partial class PasswordFieldProbeTests
{
    private const uint WS_POPUP = 0x80000000, WS_VISIBLE = 0x10000000, WS_CHILD = 0x40000000, ES_PASSWORD = 0x0020;
    private const uint WS_EX_TOOLWINDOW = 0x00000080, WS_EX_NOACTIVATE = 0x08000000;

    [LibraryImport("user32.dll", EntryPoint = "CreateWindowExW", StringMarshalling = StringMarshalling.Utf16)]
    private static partial nint CreateWindowEx(uint exStyle, string cls, string? name, uint style, int x, int y, int w, int h, nint parent, nint menu, nint inst, nint param);

    [LibraryImport("user32.dll")]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static partial bool DestroyWindow(nint hwnd);

    [LibraryImport("user32.dll", EntryPoint = "PeekMessageW")]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static unsafe partial bool PeekMessage(MSG* msg, nint hwnd, uint min, uint max, uint remove);

    private static unsafe void Pump()
    {
        MSG m;
        while (PeekMessage(&m, 0, 0, 0, 1)) { Win32.TranslateMessage(&m); Win32.DispatchMessageW(&m); }
    }

    [Fact]
    public void Finds_a_password_edit_and_its_rectangle()
    {
        nint top = 0;
        using var ready = new ManualResetEventSlim();
        using var done = new ManualResetEventSlim();
        var ui = new Thread(() =>
        {
            // "STATIC" is a registered class with no special behaviour; enough as a container.
            top = CreateWindowEx(WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE, "STATIC", "br-test", WS_POPUP | WS_VISIBLE, -30000, -30000, 400, 200, 0, 0, 0, 0);
            CreateWindowEx(0, "EDIT", "", WS_CHILD | WS_VISIBLE, 10, 10, 200, 24, top, 0, 0, 0);
            CreateWindowEx(0, "EDIT", "secret", WS_CHILD | WS_VISIBLE | ES_PASSWORD, 10, 60, 180, 26, top, 0, 0, 0);
            ready.Set();
            while (!done.IsSet)
            {
                Pump();
                Thread.Sleep(5);
            }
            DestroyWindow(top);
        }) { IsBackground = true };
        ui.SetApartmentState(ApartmentState.STA);
        ui.Start();
        Assert.True(ready.Wait(5000));
        Assert.NotEqual(0, top);

        RECT[] found = [];
        var probe = new Thread(() =>
        {
            Ole.CoInitializeEx(0, Ole.COINIT_MULTITHREADED);
            using var p = new PasswordFieldProbe();
            found = p.Find(top);
            Ole.CoUninitialize();
        });
        probe.Start();
        bool finished = probe.Join(20000);
        done.Set();
        Assert.True(finished);
        var r = Assert.Single(found);
        Assert.Equal((-30000 + 10, -30000 + 60), (r.Left, r.Top));
        Assert.Equal((180, 26), (r.Width, r.Height));
    }
}
