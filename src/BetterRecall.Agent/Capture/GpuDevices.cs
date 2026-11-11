// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Runtime.CompilerServices;
using System.Runtime.InteropServices;
using BetterRecall.Agent.Replay;
using BetterRecall.Agent.Util;
using SharpGen.Runtime;
using Vortice.Direct3D;
using Vortice.Direct3D11;
using Vortice.DXGI;

namespace BetterRecall.Agent.Capture;

internal sealed record OutputInfo(
    int Index,
    string DeviceName,
    long AdapterLuid,
    uint VendorId,
    string AdapterName,
    int Left, int Top, int Width, int Height,
    nint Monitor,
    int RefreshHz = 60);

/// <summary>Integrated: shares system memory (dedicated VRAM at most 1 GiB), the usual iGPU signature.</summary>
internal sealed record AdapterInfo(long Luid, uint VendorId, string Name, bool Software, bool Integrated = false, long VramMb = 0, uint DeviceId = 0);

/// <summary>One D3D11 device per adapter, shared by every output on it (multithread protected).</summary>
internal sealed class GpuDevice : IDisposable
{
    public required long Luid { get; init; }
    public required uint VendorId { get; init; }
    public required ID3D11Device Device { get; init; }
    public required ID3D11DeviceContext Context { get; init; }
    public required ID3D11Multithread Multithread { get; init; }
    public required ID3D11VideoDevice VideoDevice { get; init; }
    public required ID3D11VideoContext VideoContext { get; init; }

    /// <summary>Serialises immediate-context use across the threads that share this device.</summary>
    public readonly object Gate = new();

    public void Dispose()
    {
        VideoContext.Dispose();
        VideoDevice.Dispose();
        Multithread.Dispose();
        Context.Dispose();
        Device.Dispose();
    }
}

internal static class GpuDevices
{
    public const uint VendorIntel = 0x8086, VendorAmd = 0x1002, VendorNvidia = 0x10DE;

    public static long LuidToInt64<T>(T luid) where T : unmanaged => Unsafe.SizeOf<T>() == 8 ? Unsafe.As<T, long>(ref luid) : throw new InvalidOperationException("LUID size");

    /// <summary>PDH instance token for an adapter LUID, e.g. luid_0x00000000_0x0000D1A3.</summary>
    public static string PdhLuidToken(long luid) => $"luid_0x{(uint)(luid >> 32):X8}_0x{(uint)luid:X8}";

    public static List<AdapterInfo> Adapters()
    {
        var list = new List<AdapterInfo>();
        using var factory = DXGI.CreateDXGIFactory1<IDXGIFactory1>();
        for (uint i = 0; factory.EnumAdapters1(i, out var adapter).Success; i++)
        {
            using (adapter)
            {
                var d = adapter.Description1;
                long vram = (long)(d.DedicatedVideoMemory / 1048576);
                list.Add(new AdapterInfo(LuidToInt64(d.Luid), d.VendorId, d.Description, (d.Flags & AdapterFlags.Software) != 0,
                    Integrated: (d.Flags & AdapterFlags.Software) == 0 && vram <= 1024, VramMb: vram, DeviceId: d.DeviceId));
            }
        }
        return list;
    }

    /// <summary>Outputs attached to the desktop, in adapter then output order. Index is the capture.monitors index.</summary>
    public static List<OutputInfo> Outputs()
    {
        var list = new List<OutputInfo>();
        using var factory = DXGI.CreateDXGIFactory1<IDXGIFactory1>();
        int index = 0;
        for (uint a = 0; factory.EnumAdapters1(a, out var adapter).Success; a++)
        {
            using (adapter)
            {
                var ad = adapter.Description1;
                if ((ad.Flags & AdapterFlags.Software) != 0) continue;
                for (uint o = 0; adapter.EnumOutputs(o, out var output).Success; o++)
                {
                    using (output)
                    {
                        var od = output.Description;
                        if (!od.AttachedToDesktop) continue;
                        var r = od.DesktopCoordinates;
                        list.Add(new OutputInfo(index++, od.DeviceName, LuidToInt64(ad.Luid), ad.VendorId, ad.Description,
                            r.Left, r.Top, r.Right - r.Left, r.Bottom - r.Top, od.Monitor,
                            ReplayRate.NormalizeRefresh(DisplayModes.RefreshHz(od.DeviceName))));
                    }
                }
            }
        }
        return list;
    }

    public static IDXGIAdapter1? FindAdapter(IDXGIFactory1 factory, long luid)
    {
        for (uint i = 0; factory.EnumAdapters1(i, out var adapter).Success; i++)
        {
            if (LuidToInt64(adapter.Description1.Luid) == luid) return adapter;
            adapter.Dispose();
        }
        return null;
    }

    public static GpuDevice CreateDevice(long luid)
    {
        using var factory = DXGI.CreateDXGIFactory1<IDXGIFactory1>();
        using var adapter = FindAdapter(factory, luid) ?? throw new InvalidOperationException("Adapter not found.");
        var desc = adapter.Description1;
        var levels = new[] { FeatureLevel.Level_11_1, FeatureLevel.Level_11_0 };
        var flags = DeviceCreationFlags.BgraSupport | DeviceCreationFlags.VideoSupport;
        D3D11.D3D11CreateDevice(adapter, DriverType.Unknown, flags, levels, out ID3D11Device? device, out ID3D11DeviceContext? context).CheckError();
        var mt = device!.QueryInterface<ID3D11Multithread>();
        mt.SetMultithreadProtected(true);
        var vd = device.QueryInterface<ID3D11VideoDevice>();
        var vc = context!.QueryInterface<ID3D11VideoContext>();
        Log.Info($"d3d11 device on vendor 0x{desc.VendorId:X4}");
        return new GpuDevice
        {
            Luid = luid, VendorId = desc.VendorId, Device = device, Context = context!, Multithread = mt, VideoDevice = vd, VideoContext = vc,
        };
    }
}

/// <summary>Shares GpuDevice instances per adapter LUID.</summary>
internal sealed class GpuDeviceCache : IDisposable
{
    private readonly Dictionary<long, GpuDevice> _devices = new();
    private readonly object _gate = new();

    public GpuDevice Get(long luid)
    {
        lock (_gate)
        {
            if (_devices.TryGetValue(luid, out var d)) return d;
            d = GpuDevices.CreateDevice(luid);
            _devices[luid] = d;
            return d;
        }
    }

    public void Invalidate(long luid)
    {
        lock (_gate)
        {
            if (_devices.Remove(luid, out var d)) d.Dispose();
        }
    }

    public void Dispose()
    {
        lock (_gate)
        {
            foreach (var d in _devices.Values) d.Dispose();
            _devices.Clear();
        }
    }
}

internal static unsafe partial class DisplayModes
{
    [LibraryImport("user32.dll", EntryPoint = "EnumDisplaySettingsW", StringMarshalling = StringMarshalling.Utf16)]
    private static partial int EnumDisplaySettingsW(string deviceName, int modeNum, byte* devMode);

    public static int RefreshHz(string deviceName)
    {
        byte* devMode = stackalloc byte[220];
        NativeMemory.Clear(devMode, 220);
        *(ushort*)(devMode + 68) = 220;
        if (EnumDisplaySettingsW(deviceName, -1, devMode) != 0)
        {
            return (int)*(uint*)(devMode + 184);
        }
        return 0;
    }
}
