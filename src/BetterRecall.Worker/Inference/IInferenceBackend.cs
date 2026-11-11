// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
namespace BetterRecall.Worker.Inference;

public enum DeviceKind { Npu, Gpu, Cpu }

/// <summary>A device a backend can compile for. Id is backend specific ("NPU", "GPU.0", "CPU", "DML:0", "QNN"); for
/// DirectML the number is the DXGI EnumAdapters1 index. Integrated: OpenVINO GPU DEVICE_TYPE ("integrated" = true,
/// "discrete" = false), null when unknown or not a GPU.</summary>
public sealed record DeviceInfo(DeviceKind Kind, string Id, string Name, string Ep, bool? Integrated = null)
{
    public string Label => Kind switch { DeviceKind.Npu => "NPU", DeviceKind.Gpu => "GPU", _ => "CPU" };
}

public enum TensorType { Float32, Int64, Int32, Float16, Other }

public sealed record TensorSpec(string Name, TensorType Type, long[] Shape)
{
    public bool IsStatic => Shape.All(d => d > 0);
    public long Elements => Shape.Aggregate(1L, (a, b) => a * Math.Max(b, 1));
}

/// <summary>Host tensor. Exactly one of Floats / Longs is set.</summary>
public sealed class HostTensor
{
    public required string Name { get; init; }
    public required long[] Shape { get; init; }
    public float[]? Floats { get; init; }
    public long[]? Longs { get; init; }

    public static HostTensor F(string name, float[] data, params long[] shape) => new() { Name = name, Shape = shape, Floats = data };
    public static HostTensor L(string name, long[] data, params long[] shape) => new() { Name = name, Shape = shape, Longs = data };
}

public interface IInferenceSession : IDisposable
{
    DeviceInfo Device { get; }
    IReadOnlyList<TensorSpec> Inputs { get; }
    IReadOnlyList<TensorSpec> Outputs { get; }

    /// <summary>Runs once. Outputs come back as float32 (int outputs are not used by the worker's graphs).</summary>
    IReadOnlyDictionary<string, HostTensor> Run(IReadOnlyList<HostTensor> inputs);
}

/// <summary>An inference runtime. OpenVINO (primary on Intel) or ONNX Runtime (CPU, DirectML, QNN).</summary>
public interface IInferenceBackend : IDisposable
{
    string Name { get; }               // OpenVINO | ONNX Runtime
    string Version { get; }
    /// <summary>NPU driver version, empty when the backend sees no NPU.</summary>
    string NpuDriverVersion { get; }
    IReadOnlyList<DeviceInfo> Devices { get; }

    /// <summary>Loads and compiles one graph. cacheDir (compiled blobs) may be null. staticShapes fixes dynamic
    /// inputs (by name) before compiling, so one dynamic graph serves several static lengths on the NPU.
    /// Throws DeviceFailedException.</summary>
    IInferenceSession Load(string modelPath, DeviceInfo device, string? cacheDir, IReadOnlyDictionary<string, long[]>? staticShapes = null);
}
