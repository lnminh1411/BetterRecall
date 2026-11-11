// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Runtime.InteropServices;
using BetterRecall.Worker.Hosting;

namespace BetterRecall.Worker.Inference;

/// <summary>One DXGI adapter. Index is the IDXGIFactory1::EnumAdapters1 index, which ONNX Runtime's DirectML EP takes
/// as device_id.</summary>
public sealed record DxgiAdapter(int Index, string Description, ulong DedicatedVideoMemory, uint Flags, uint VendorId, uint DeviceId = 0)
{
    public const uint FlagSoftware = 2;        // DXGI_ADAPTER_FLAG_SOFTWARE
    public const uint VendorIntel = 0x8086;
    public const uint VendorMicrosoft = 0x1414;

    public bool IsSoftware => (Flags & FlagSoftware) != 0;
    public bool IsBasicRender => VendorId == VendorMicrosoft || Description.Contains("Basic Render", StringComparison.OrdinalIgnoreCase);
    /// <summary>Intel with under 512 MB dedicated memory: an integrated GPU (Arc cards have GBs).</summary>
    public bool IsIntelIntegrated => VendorId == VendorIntel && DedicatedVideoMemory < 512UL * 1024 * 1024;
}

/// <summary>DXGI adapter list for the NSFW gate's DirectML device (worker side; the agent uses Vortice).</summary>
public static unsafe partial class DxgiAdapters
{
    private static readonly Guid IidFactory1 = new("770aae78-f26f-4dba-a829-253c83d1b387");
    private const int DxgiErrorNotFound = unchecked((int)0x887A0002);

    [LibraryImport("dxgi.dll")]
    private static partial int CreateDXGIFactory1(in Guid riid, out nint factory);

    [StructLayout(LayoutKind.Sequential)]
    private struct DXGI_ADAPTER_DESC1
    {
        public fixed char Description[128];
        public uint VendorId, DeviceId, SubSysId, Revision;
        public nuint DedicatedVideoMemory, DedicatedSystemMemory, SharedSystemMemory;
        public uint LuidLow;
        public int LuidHigh;
        public uint Flags;
    }

    /// <summary>"auto": the first adapter that is not software, not the Microsoft Basic Render Driver and not an Intel
    /// integrated GPU (the iGPU has its own slot through OpenVINO). Otherwise the first adapter whose description
    /// contains the setting (case-insensitive). Null when nothing matches.</summary>
    public static DxgiAdapter? Pick(string? setting, IReadOnlyList<DxgiAdapter> adapters)
    {
        var s = setting?.Trim();
        if (string.IsNullOrEmpty(s) || string.Equals(s, "auto", StringComparison.OrdinalIgnoreCase))
            return adapters.FirstOrDefault(a => !a.IsSoftware && !a.IsBasicRender && !a.IsIntelIntegrated);
        return adapters.FirstOrDefault(a => a.Description.Contains(s, StringComparison.OrdinalIgnoreCase));
    }

    /// <summary>Every adapter in EnumAdapters1 order; empty when DXGI is unavailable.</summary>
    public static IReadOnlyList<DxgiAdapter> Enumerate()
    {
        var list = new List<DxgiAdapter>();
        nint factory = 0;
        try
        {
            if (CreateDXGIFactory1(IidFactory1, out factory) < 0 || factory == 0) return list;
            var vtbl = *(nint**)factory;
            var enumAdapters1 = (delegate* unmanaged[Stdcall]<nint, uint, nint*, int>)vtbl[12];
            for (uint i = 0; i < 64; i++)
            {
                nint adapter = 0;
                int hr = enumAdapters1(factory, i, &adapter);
                if (hr == DxgiErrorNotFound || hr < 0 || adapter == 0) break;
                try
                {
                    var avtbl = *(nint**)adapter;
                    var getDesc1 = (delegate* unmanaged[Stdcall]<nint, DXGI_ADAPTER_DESC1*, int>)avtbl[10];
                    DXGI_ADAPTER_DESC1 d;
                    if (getDesc1(adapter, &d) < 0) continue;
                    string name = new string(d.Description).TrimEnd('\0').Trim();
                    list.Add(new DxgiAdapter((int)i, name, d.DedicatedVideoMemory, d.Flags, d.VendorId, d.DeviceId));
                }
                finally { Release(adapter); }
            }
        }
        catch (Exception e) when (e is DllNotFoundException or EntryPointNotFoundException or SEHException)
        {
            Log.Info($"DXGI unavailable: {e.GetType().Name}");
        }
        finally { if (factory != 0) Release(factory); }
        return list;
    }

    private static void Release(nint unk)
    {
        var vtbl = *(nint**)unk;
        ((delegate* unmanaged[Stdcall]<nint, uint>)vtbl[2])(unk);
    }
}
