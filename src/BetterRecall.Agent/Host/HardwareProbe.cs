// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Runtime.InteropServices;
using BetterRecall.Agent.Capture;
using BetterRecall.Agent.Native;
using BetterRecall.Agent.Replay;
using BetterRecall.Contracts;
using Microsoft.Win32;
using Vortice.DXGI;

namespace BetterRecall.Agent.Host;

internal static unsafe partial class HardwareProbe
{
    public static HardwareInfo Collect(IReadOnlyList<OutputInfo> outputs)
    {
        string cpu = ProbeCpu();
        int cores = ProbeCores();
        int threads = Environment.ProcessorCount;
        long ramMb = ProbeRamMb();
        bool hasBattery = ProbeBattery();
        GpuInfo[] gpus = ProbeGpus();
        NpuInfo? npu = ProbeNpu();
        MonitorInfo[] monitors = ProbeMonitors(outputs);
        string os = $"{RuntimeInformation.OSDescription} ({Environment.OSVersion.Version})";

        return new HardwareInfo(cpu, cores, threads, ramMb, hasBattery, gpus, npu, monitors, os);
    }

    private static string ProbeCpu()
    {
        try
        {
            using var key = Registry.LocalMachine.OpenSubKey(@"HARDWARE\DESCRIPTION\System\CentralProcessor\0");
            return (key?.GetValue("ProcessorNameString") as string)?.Trim() ?? "";
        }
        catch
        {
            return "";
        }
    }

    [LibraryImport("kernel32.dll", SetLastError = true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static partial bool GetLogicalProcessorInformationEx(int relationshipType, byte* buffer, ref int returnedLength);

    private static int ProbeCores()
    {
        try
        {
            int length = 0;
            GetLogicalProcessorInformationEx(0, null, ref length);
            if (length <= 0) return Environment.ProcessorCount;

            byte* buffer = stackalloc byte[Math.Min(length, 4096)];
            byte* heapBuf = null;
            if (length > 4096)
            {
                heapBuf = (byte*)NativeMemory.Alloc((nuint)length);
                buffer = heapBuf;
            }

            try
            {
                if (!GetLogicalProcessorInformationEx(0, buffer, ref length))
                    return Environment.ProcessorCount;

                int cores = 0;
                int offset = 0;
                while (offset + 8 <= length)
                {
                    uint relationship = *(uint*)(buffer + offset);
                    uint size = *(uint*)(buffer + offset + 4);
                    if (size == 0) break;
                    if (relationship == 0) cores++;
                    offset += (int)size;
                }
                return cores > 0 ? cores : Environment.ProcessorCount;
            }
            finally
            {
                if (heapBuf != null) NativeMemory.Free(heapBuf);
            }
        }
        catch
        {
            return Environment.ProcessorCount;
        }
    }

    [StructLayout(LayoutKind.Sequential)]
    private struct MEMORYSTATUSEX
    {
        public uint dwLength;
        public uint dwMemoryLoad;
        public ulong ullTotalPhys;
        public ulong ullAvailPhys;
        public ulong ullTotalPageFile;
        public ulong ullAvailPageFile;
        public ulong ullTotalVirtual;
        public ulong ullAvailVirtual;
        public ulong ullAvailExtendedVirtual;
    }

    [LibraryImport("kernel32.dll", SetLastError = true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static partial bool GlobalMemoryStatusEx(MEMORYSTATUSEX* lpBuffer);

    private static long ProbeRamMb()
    {
        try
        {
            MEMORYSTATUSEX mem = new() { dwLength = (uint)sizeof(MEMORYSTATUSEX) };
            if (GlobalMemoryStatusEx(&mem))
            {
                return (long)(mem.ullTotalPhys / (1024 * 1024));
            }
            return 0;
        }
        catch
        {
            return 0;
        }
    }

    private static bool ProbeBattery()
    {
        try
        {
            SYSTEM_POWER_STATUS status;
            if (Win32.GetSystemPowerStatus(&status))
            {
                return status.BatteryFlag != 128 && status.BatteryFlag != 255;
            }
            return false;
        }
        catch
        {
            return false;
        }
    }

    private static string ProbeDriver(long luid)
    {
        try
        {
            using var factory = DXGI.CreateDXGIFactory1<IDXGIFactory1>();
            using var adapter = GpuDevices.FindAdapter(factory, luid);
            if (adapter is not null && adapter.CheckInterfaceSupport<IDXGIDevice>(out long umdVersion))
            {
                ushort a = (ushort)(umdVersion >> 48);
                ushort b = (ushort)(umdVersion >> 32);
                ushort c = (ushort)(umdVersion >> 16);
                ushort d = (ushort)umdVersion;
                return $"{a}.{b}.{c}.{d}";
            }
            return "";
        }
        catch
        {
            return "";
        }
    }

    private static GpuInfo[] ProbeGpus()
    {
        try
        {
            var list = new List<GpuInfo>();
            foreach (var a in GpuDevices.Adapters())
            {
                // Software adapters and the Microsoft Basic Render Driver (vendor 0x1414) are no GPU to pick for
                // inference or the NSFW gate (the encoder plan enumerates adapters itself).
                if (a.Software || a.VendorId == 0x1414) continue;
                string vendor = a.VendorId switch
                {
                    GpuDevices.VendorIntel => "Intel",
                    GpuDevices.VendorAmd => "AMD",
                    GpuDevices.VendorNvidia => "NVIDIA",
                    _ => $"0x{a.VendorId:X}",
                };
                string driver = ProbeDriver(a.Luid);
                string[] codecs = EncoderFactory.Codecs(a);
                list.Add(new GpuInfo(a.Name, vendor, a.Integrated, a.VramMb, $"0x{a.Luid:X16}", driver, codecs));
            }
            return list.ToArray();
        }
        catch
        {
            return [];
        }
    }

    [StructLayout(LayoutKind.Sequential)]
    private struct SP_DEVINFO_DATA
    {
        public uint cbSize;
        public Guid ClassGuid;
        public uint DevInst;
        public nuint Reserved;
    }

    [LibraryImport("setupapi.dll", EntryPoint = "SetupDiGetClassDevsW", SetLastError = true)]
    private static partial nint SetupDiGetClassDevsW(Guid* classGuid, char* enumerator, nint hwndParent, uint flags);

    [LibraryImport("setupapi.dll", EntryPoint = "SetupDiEnumDeviceInfo", SetLastError = true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static partial bool SetupDiEnumDeviceInfo(nint deviceInfoSet, uint memberIndex, SP_DEVINFO_DATA* deviceInfoData);

    [LibraryImport("setupapi.dll", EntryPoint = "SetupDiGetDeviceRegistryPropertyW", SetLastError = true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static partial bool SetupDiGetDeviceRegistryPropertyW(
        nint deviceInfoSet,
        SP_DEVINFO_DATA* deviceInfoData,
        uint property,
        uint* propertyRegDataType,
        byte* propertyBuffer,
        uint propertyBufferSize,
        uint* requiredSize);

    [LibraryImport("setupapi.dll", EntryPoint = "SetupDiDestroyDeviceInfoList", SetLastError = true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static partial bool SetupDiDestroyDeviceInfoList(nint deviceInfoSet);

    private static string? GetDeviceProperty(nint set, SP_DEVINFO_DATA* dev, uint prop)
    {
        uint req = 0;
        uint regType = 0;
        char* buf = stackalloc char[256];
        if (SetupDiGetDeviceRegistryPropertyW(set, dev, prop, &regType, (byte*)buf, 512, &req))
        {
            return new string(buf).TrimEnd('\0').Trim();
        }
        if (req > 512)
        {
            byte* heapBuf = (byte*)NativeMemory.Alloc(req);
            try
            {
                if (SetupDiGetDeviceRegistryPropertyW(set, dev, prop, &regType, heapBuf, req, &req))
                {
                    return new string((char*)heapBuf).TrimEnd('\0').Trim();
                }
            }
            finally
            {
                NativeMemory.Free(heapBuf);
            }
        }
        return null;
    }

    private static NpuInfo? ProbeNpu()
    {
        try
        {
            Guid guid = new("f01a9d53-3ff6-48d2-9f97-c8a7004be10c");
            nint set = SetupDiGetClassDevsW(&guid, null, 0, 2); // DIGCF_PRESENT = 2
            if (set == 0 || set == (nint)(-1)) return null;

            try
            {
                SP_DEVINFO_DATA data = new() { cbSize = (uint)sizeof(SP_DEVINFO_DATA) };
                if (!SetupDiEnumDeviceInfo(set, 0, &data)) return null;

                string name = GetDeviceProperty(set, &data, 0x0C) // SPDRP_FRIENDLYNAME
                              ?? GetDeviceProperty(set, &data, 0x00) // SPDRP_DEVICEDESC
                              ?? "";

                string? driver = GetDeviceProperty(set, &data, 0x09); // SPDRP_DRIVER
                string driverVersion = "";
                if (!string.IsNullOrEmpty(driver))
                {
                    using var key = Registry.LocalMachine.OpenSubKey(@"SYSTEM\CurrentControlSet\Control\Class\" + driver);
                    driverVersion = (key?.GetValue("DriverVersion") as string)?.Trim() ?? "";
                }

                return new NpuInfo(name, driverVersion);
            }
            finally
            {
                SetupDiDestroyDeviceInfoList(set);
            }
        }
        catch
        {
            return null;
        }
    }

    private static MonitorInfo[] ProbeMonitors(IReadOnlyList<OutputInfo> outputs)
    {
        try
        {
            return outputs.Select(o => new MonitorInfo(o.Index, o.Width, o.Height, o.RefreshHz, o.AdapterName)).ToArray();
        }
        catch
        {
            return [];
        }
    }
}
