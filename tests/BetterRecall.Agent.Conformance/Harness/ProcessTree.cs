// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Diagnostics;
using System.Runtime.InteropServices;

namespace BetterRecall.Agent.Conformance.Harness;

internal static class ProcessTree
{
    private const uint Th32csSnapProcess = 0x00000002;

    [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode)]
    private struct PROCESSENTRY32W
    {
        public uint dwSize;
        public uint cntUsage;
        public uint th32ProcessID;
        public nuint th32DefaultHeapID;
        public uint th32ModuleID;
        public uint cntThreads;
        public uint th32ParentProcessID;
        public int pcPriClassBase;
        public uint dwFlags;
        [MarshalAs(UnmanagedType.ByValTStr, SizeConst = 260)]
        public string szExeFile;
    }

    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern nint CreateToolhelp32Snapshot(uint dwFlags, uint th32ProcessID);

    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static extern bool Process32FirstW(nint hSnapshot, ref PROCESSENTRY32W lppe);

    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static extern bool Process32NextW(nint hSnapshot, ref PROCESSENTRY32W lppe);

    [DllImport("kernel32.dll", SetLastError = true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static extern bool CloseHandle(nint hObject);

    public sealed record ChildProcess(int Pid, string ExeName);

    public static IReadOnlyList<ChildProcess> GetDirectChildren(int parentPid)
    {
        var all = Snapshot();
        return all.Where(e => e.ParentPid == parentPid).Select(e => new ChildProcess(e.Pid, e.ExeName)).ToList();
    }

    public static IReadOnlyList<int> GetDescendantPids(int rootPid)
    {
        var all = Snapshot();
        var byParent = all.GroupBy(e => e.ParentPid).ToDictionary(g => g.Key, g => g.Select(x => x.Pid).ToList());
        var result = new List<int>();
        var queue = new Queue<int>();
        var seen = new HashSet<int> { rootPid };
        queue.Enqueue(rootPid);
        while (queue.Count > 0)
        {
            var cur = queue.Dequeue();
            if (!byParent.TryGetValue(cur, out var kids)) continue;
            foreach (var k in kids)
            {
                if (!seen.Add(k)) continue;
                result.Add(k);
                queue.Enqueue(k);
            }
        }
        return result;
    }

    public static bool IsAlive(int pid)
    {
        try
        {
            using var p = Process.GetProcessById(pid);
            return !p.HasExited;
        }
        catch (ArgumentException) { return false; }
        catch (InvalidOperationException) { return false; }
    }

    public static bool WaitForExitByPid(int pid, int timeoutMs)
    {
        try
        {
            using var p = Process.GetProcessById(pid);
            return p.WaitForExit(timeoutMs);
        }
        catch (ArgumentException) { return true; }
        catch (InvalidOperationException) { return true; }
    }

    public static void KillPid(int pid)
    {
        try
        {
            using var p = Process.GetProcessById(pid);
            if (!p.HasExited) p.Kill();
        }
        catch (ArgumentException) { }
        catch (InvalidOperationException) { }
        catch (System.ComponentModel.Win32Exception) { }
    }

    private static List<(int Pid, int ParentPid, string ExeName)> Snapshot()
    {
        var list = new List<(int, int, string)>();
        nint snap = CreateToolhelp32Snapshot(Th32csSnapProcess, 0);
        if (snap == -1) return list;
        try
        {
            var pe = new PROCESSENTRY32W { dwSize = (uint)Marshal.SizeOf<PROCESSENTRY32W>() };
            if (!Process32FirstW(snap, ref pe)) return list;
            do
            {
                list.Add(((int)pe.th32ProcessID, (int)pe.th32ParentProcessID, pe.szExeFile ?? ""));
            }
            while (Process32NextW(snap, ref pe));
        }
        finally
        {
            CloseHandle(snap);
        }
        return list;
    }
}
