// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Runtime.InteropServices;
using BetterRecall.Agent.Util;

namespace BetterRecall.Agent.Capture;

/// <summary>
/// capture.monitors holds enumeration indices (what the settings page shows). A hot-plug, lid close or adapter reorder
/// renumbers outputs, so the index list would silently move to another monitor (A12). When the setting is applied the
/// chosen indices are pinned to stable monitor ids (the monitor's device interface path, which carries its EDID
/// vendor/product code and connector) and saved; later rebuilds capture by id while the setting is unchanged. A changed
/// setting, or one written before this existed, is read as indices against the current enumeration and pinned again.
/// </summary>
internal static unsafe partial class MonitorSelection
{
    public const string FileName = "capture-monitors.txt";

    /// <summary>The settings value the ids were resolved from, and the ids it meant at that time.</summary>
    internal sealed record Pin(int[] Indices, string[] Ids);

    /// <summary>The outputs to capture, and the pin to save (null: nothing to save). Pure.</summary>
    public static (List<OutputInfo> Selected, Pin? Save) Resolve(int[] setting, IReadOnlyList<OutputInfo> outputs, Func<OutputInfo, string> idOf, Pin? pinned)
    {
        if (setting.Length == 0) return (outputs.ToList(), null);   // all monitors: nothing to pin
        if (pinned is not null && pinned.Indices.SequenceEqual(setting))
        {
            var ids = new HashSet<string>(pinned.Ids, StringComparer.OrdinalIgnoreCase);
            return (outputs.Where(o => ids.Contains(idOf(o))).ToList(), null);
        }
        var chosen = outputs.Where(o => setting.Contains(o.Index)).ToList();
        return (chosen, new Pin(setting.ToArray(), chosen.Select(idOf).ToArray()));
    }

    public static Pin? Load(string? path)
    {
        try
        {
            if (path is null || !File.Exists(path)) return null;
            var lines = File.ReadAllLines(path);
            if (lines.Length == 0) return null;
            var idx = lines[0].Split(',', StringSplitOptions.RemoveEmptyEntries | StringSplitOptions.TrimEntries)
                .Select(int.Parse).ToArray();
            return new Pin(idx, lines.Skip(1).Where(l => l.Length > 0).ToArray());
        }
        catch (Exception ex) { Log.Warn("capture monitor ids unreadable", ex); return null; }
    }

    public static void Save(string? path, Pin pin)
    {
        if (path is null) return;
        try
        {
            var tmp = path + ".tmp";
            File.WriteAllLines(tmp, [string.Join(",", pin.Indices), .. pin.Ids]);
            File.Move(tmp, path, overwrite: true);
        }
        catch (Exception ex) { Log.Warn("capture monitor ids not saved", ex); }
    }

    /// <summary>Stable id of the monitor on a DXGI output (\\.\DISPLAYn): its device interface path
    /// (\\?\DISPLAY#&lt;EDID id&gt;#&lt;instance&gt;#{guid}), else the GDI device name.</summary>
    public static string StableId(OutputInfo o) => DeviceInterfacePath(o.DeviceName) ?? o.DeviceName;

    private static string? DeviceInterfacePath(string adapterDevice)
    {
        try
        {
            var dd = new DISPLAY_DEVICEW { cb = (uint)sizeof(DISPLAY_DEVICEW) };
            // Monitor 0 on that output; EDD_GET_DEVICE_INTERFACE_NAME puts the interface path in DeviceID.
            if (!EnumDisplayDevicesW(adapterDevice, 0, &dd, 1)) return null;
            var id = new string(dd.DeviceID);
            return id.Length > 0 ? id : null;
        }
        catch { return null; }
    }

    [StructLayout(LayoutKind.Sequential)]
    private struct DISPLAY_DEVICEW
    {
        public uint cb;
        public fixed char DeviceName[32];
        public fixed char DeviceString[128];
        public uint StateFlags;
        public fixed char DeviceID[128];
        public fixed char DeviceKey[128];
    }

    [LibraryImport("user32.dll", StringMarshalling = StringMarshalling.Utf16)]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static partial bool EnumDisplayDevicesW(string lpDevice, uint iDevNum, DISPLAY_DEVICEW* lpDisplayDevice, uint dwFlags);
}
