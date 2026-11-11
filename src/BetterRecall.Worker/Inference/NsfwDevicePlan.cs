// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
namespace BetterRecall.Worker.Inference;

/// <summary>Device order of the NSFW gate (classifier and detector), setting privacy.nsfwDevice. Pure.
/// auto: NPU, integrated GPU (OpenVINO), discrete GPU (DirectML on the chosen DXGI adapter), CPU (ONNX Runtime).
/// npu / igpu / gpu: that device, then the CPU. cpu: the CPU only.</summary>
public static class NsfwDevicePlan
{
    public static readonly string[] Values = ["auto", "npu", "igpu", "gpu", "cpu"];

    public enum Slot { Npu, IntegratedGpu, DiscreteGpu, Cpu }

    /// <summary>Slot of a device, or null when the gate never uses it (an OpenVINO GPU not reported as integrated,
    /// QNN, an OpenVINO CPU).</summary>
    public static Slot? SlotOf(DeviceInfo d) => d.Kind switch
    {
        DeviceKind.Npu when d.Ep == "OpenVINO" => Slot.Npu,
        DeviceKind.Gpu when d.Ep == "OpenVINO" && d.Integrated == true => Slot.IntegratedGpu,
        DeviceKind.Gpu when d.Ep == "DirectML" => Slot.DiscreteGpu,
        DeviceKind.Cpu when d.Ep == "CPU" => Slot.Cpu,
        _ => null,
    };

    public static IReadOnlyList<Slot> Slots(string setting) => setting switch
    {
        "npu" => [Slot.Npu, Slot.Cpu],
        "igpu" => [Slot.IntegratedGpu, Slot.Cpu],
        "gpu" => [Slot.DiscreteGpu, Slot.Cpu],
        "cpu" => [Slot.Cpu],
        _ => [Slot.Npu, Slot.IntegratedGpu, Slot.DiscreteGpu, Slot.Cpu],
    };

    /// <summary>Candidates in try order: for each slot of the setting, the first available device in that slot.
    /// Slots with no device are skipped.</summary>
    public static IReadOnlyList<DeviceInfo> Order(string setting, IReadOnlyList<DeviceInfo> available)
    {
        var list = new List<DeviceInfo>();
        foreach (var slot in Slots(setting))
            if (available.FirstOrDefault(d => SlotOf(d) == slot) is { } dev) list.Add(dev);
        return list;
    }

    /// <summary>Name shown in hello.nsfwDevice and logs: NPU, iGPU, GPU (adapter name) or CPU.</summary>
    public static string Label(DeviceInfo d) => SlotOf(d) switch
    {
        Slot.Npu => "NPU",
        Slot.IntegratedGpu => "iGPU",
        Slot.DiscreteGpu => $"GPU ({d.Name})",
        _ => "CPU",
    };
}
