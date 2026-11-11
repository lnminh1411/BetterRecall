// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Runtime.InteropServices;
using BetterRecall.Worker.Hosting;
using BetterRecall.Worker.Protocol;
using static BetterRecall.Worker.Inference.OpenVino.OvNative;

namespace BetterRecall.Worker.Inference.OpenVino;

/// <summary>OpenVINO through its C API. NPU, Intel GPU (only devices whose name says Intel: the dGPUs that OpenVINO's
/// OpenCL path also lists are never used), CPU.</summary>
public sealed unsafe class OpenVinoBackend : IInferenceBackend
{
    private readonly nint _core;
    private readonly Dictionary<string, HashSet<string>> _supported = new(StringComparer.Ordinal);

    public string Name => "OpenVINO";
    public string Version { get; }
    public string NpuDriverVersion { get; }
    public IReadOnlyList<DeviceInfo> Devices { get; }

    private OpenVinoBackend(nint core, string version, string npuDriver, IReadOnlyList<DeviceInfo> devices)
    {
        _core = core; Version = version; NpuDriverVersion = npuDriver; Devices = devices;
    }

    /// <summary>Null when openvino_c.dll cannot be loaded.</summary>
    public static OpenVinoBackend? TryCreate()
    {
        try
        {
            ov_version_t ver;
            Check(ov_get_openvino_version(&ver), "version");
            string version = Marshal.PtrToStringUTF8(ver.buildNumber) ?? "unknown";
            ov_version_free(&ver);

            Check(ov_core_create(out var core), "core");
            var names = new List<string>();
            ov_available_devices_t devs;
            if (ov_core_get_available_devices(core, &devs) == OK)
            {
                for (nuint i = 0; i < devs.size; i++) names.Add(Marshal.PtrToStringUTF8(devs.devices[i]) ?? "");
                ov_available_devices_free(&devs);
            }

            var devices = new List<DeviceInfo>();
            string npuDriver = "";
            foreach (var n in names)
            {
                string full = GetProp(core, n, "FULL_DEVICE_NAME") ?? n;
                if (n.StartsWith("NPU", StringComparison.Ordinal))
                {
                    devices.Add(new DeviceInfo(DeviceKind.Npu, n, full, "OpenVINO"));
                    npuDriver = GetProp(core, n, "NPU_DRIVER_VERSION") ?? "";
                }
                else if (n.StartsWith("GPU", StringComparison.Ordinal))
                {
                    if (full.Contains("Intel", StringComparison.OrdinalIgnoreCase))
                    {
                        // DEVICE_TYPE: "integrated" | "discrete". The NSFW gate's device order needs the distinction.
                        string? type = GetProp(core, n, "DEVICE_TYPE");
                        bool? integrated = type is null ? null : string.Equals(type.Trim(), "integrated", StringComparison.OrdinalIgnoreCase);
                        devices.Add(new DeviceInfo(DeviceKind.Gpu, n, full, "OpenVINO", integrated));
                    }
                }
                else if (n == "CPU") devices.Add(new DeviceInfo(DeviceKind.Cpu, n, full, "OpenVINO"));
            }
            Log.Info($"OpenVINO {version}: devices [{string.Join(", ", devices.Select(d => d.Integrated is { } i ? $"{d.Id}({(i ? "integrated" : "discrete")})" : d.Id))}] npuDriver={npuDriver}");
            return new OpenVinoBackend(core, version, npuDriver, devices);
        }
        catch (Exception e) when (e is DllNotFoundException or EntryPointNotFoundException or OpenVinoException or BadImageFormatException)
        {
            Log.Info($"OpenVINO unavailable: {e.GetType().Name}");
            return null;
        }
    }

    private static string? GetProp(nint core, string device, string key)
        => ov_core_get_property(core, device, key, out var p) == OK ? TakeString(p) : null;

    private HashSet<string> Supported(string device)
    {
        if (_supported.TryGetValue(device, out var s)) return s;
        var raw = GetProp(_core, device, "SUPPORTED_PROPERTIES") ?? "";
        s = new HashSet<string>(raw.Split([' ', ',', '\t', '\n'], StringSplitOptions.RemoveEmptyEntries), StringComparer.Ordinal);
        _supported[device] = s;
        return s;
    }

    /// <summary>Background-friendly compile properties per device (research.md 2.3), filtered to what the plugin supports.</summary>
    internal static List<(string Key, string Value)> PropertiesFor(DeviceKind kind, string? cacheDir)
    {
        var p = new List<(string, string)> { ("PERFORMANCE_HINT", "LATENCY"), ("PERFORMANCE_HINT_NUM_REQUESTS", "1") };
        switch (kind)
        {
            case DeviceKind.Npu:
                p.Add(("NPU_TURBO", "NO"));
                p.Add(("WORKLOAD_TYPE", "EFFICIENT"));
                p.Add(("MODEL_PRIORITY", "LOW"));
                break;
            case DeviceKind.Gpu:
                p.Add(("GPU_QUEUE_THROTTLE", "LOW"));
                p.Add(("GPU_QUEUE_PRIORITY", "LOW"));
                p.Add(("MODEL_PRIORITY", "LOW"));
                p.Add(("INFERENCE_PRECISION_HINT", "f32")); // EmbeddingGemma 2: float16 gives NaN (model card)
                break;
            case DeviceKind.Cpu:
                p.Add(("INFERENCE_NUM_THREADS", Math.Clamp(Environment.ProcessorCount / 4, 2, 4).ToString()));
                p.Add(("SCHEDULING_CORE_TYPE", "ECORE_ONLY"));
                p.Add(("INFERENCE_PRECISION_HINT", "f32"));
                p.Add(("ENABLE_CPU_PINNING", "NO"));
                break;
        }
        if (!string.IsNullOrEmpty(cacheDir)) p.Add(("CACHE_DIR", cacheDir));
        return p;
    }

    public IInferenceSession Load(string modelPath, DeviceInfo device, string? cacheDir, IReadOnlyDictionary<string, long[]>? staticShapes = null)
    {
        nint model = 0, compiled = 0;
        try
        {
            string? bin = modelPath.EndsWith(".xml", StringComparison.OrdinalIgnoreCase) ? Path.ChangeExtension(modelPath, ".bin") : null;
            if (bin is not null && !File.Exists(bin)) bin = null;
            Check(ov_core_read_model_unicode(_core, modelPath, bin, out model), "read model");
            if (staticShapes is not null)
                foreach (var (name, dims) in staticShapes)
                {
                    ov_partial_shape_t ps;
                    fixed (long* d = dims) Check(ov_partial_shape_create_static(dims.Length, d, &ps), "partial shape");
                    try { Check(ov_model_reshape_input_by_name(model, name, ps), $"reshape {name}"); }
                    finally { ov_partial_shape_free(&ps); }
                }
            var inputs = Ports(model, input: true);
            var outputs = Ports(model, input: false);
            // The NPU needs static inputs and outputs. Inner nodes may still look dynamic after a reshape (shape
            // subgraphs of ONNX exports such as PaddleOCR); the NPU compiler folds those, so only the ports are checked.
            if (device.Kind == DeviceKind.Npu && inputs.Concat(outputs).Any(p => p.Shape.Any(d => d < 0)))
                throw new DeviceFailedException("NPU needs a static-shape model");

            if (cacheDir is not null) Directory.CreateDirectory(cacheDir);
            var supported = Supported(device.Id);
            var props = PropertiesFor(device.Kind, cacheDir).Where(kv => supported.Count == 0 || supported.Contains(kv.Key)).ToList();
            var native = new List<nint>();
            try
            {
                var arr = stackalloc ov_property_t[props.Count];
                for (int i = 0; i < props.Count; i++)
                {
                    var k = Marshal.StringToCoTaskMemUTF8(props[i].Key); native.Add(k);
                    var v = Marshal.StringToCoTaskMemUTF8(props[i].Value); native.Add(v);
                    arr[i] = new ov_property_t { key = k, value = v };
                }
                int st = ov_core_compile_model_props(_core, model, device.Id, (nuint)props.Count, arr, out compiled);
                if (st != OK) throw new DeviceFailedException($"{device.Label} compile failed: {LastError(st)}");
            }
            finally { foreach (var n in native) Marshal.FreeCoTaskMem(n); }

            Check(ov_compiled_model_create_infer_request(compiled, out var request), "infer request");
            return new OvSession(device, compiled, request, inputs, outputs);
        }
        catch (OpenVinoException e)
        {
            if (compiled != 0) ov_compiled_model_free(compiled);
            throw new DeviceFailedException($"{device.Label} {e.Message}");
        }
        catch
        {
            if (compiled != 0) ov_compiled_model_free(compiled);
            throw;
        }
        finally
        {
            if (model != 0) ov_model_free(model);
        }
    }

    private static List<TensorSpec> Ports(nint model, bool input)
    {
        Check(input ? ov_model_inputs_size(model, out var n) : ov_model_outputs_size(model, out n), "ports");
        var list = new List<TensorSpec>();
        for (nuint i = 0; i < n; i++)
        {
            Check(input ? ov_model_const_input_by_index(model, i, out var port) : ov_model_const_output_by_index(model, i, out port), "port");
            try
            {
                string name = ov_port_get_any_name(port, out var np) == OK ? TakeString(np) : $"{(input ? "in" : "out")}{i}";
                ov_port_get_element_type(port, out var et);
                long[] dims = [];
                ov_shape_t shape;
                if (ov_const_port_get_shape(port, &shape) == OK)
                {
                    dims = new long[shape.rank];
                    for (int d = 0; d < shape.rank; d++) dims[d] = shape.dims[d];
                    ov_shape_free(&shape);
                }
                else dims = [-1]; // dynamic
                list.Add(new TensorSpec(name, Map(et), dims));
            }
            finally { ov_output_const_port_free(port); }
        }
        return list;
    }

    internal static TensorType Map(int et) => et switch
    {
        ET_F32 => TensorType.Float32,
        ET_I64 => TensorType.Int64,
        ET_I32 => TensorType.Int32,
        ET_F16 => TensorType.Float16,
        _ => TensorType.Other,
    };

    public void Dispose()
    {
        if (_core != 0) ov_core_free(_core);
    }

    private sealed class OvSession(DeviceInfo device, nint compiled, nint request, List<TensorSpec> inputs, List<TensorSpec> outputs) : IInferenceSession
    {
        private nint _compiled = compiled, _request = request;
        public DeviceInfo Device { get; } = device;
        public IReadOnlyList<TensorSpec> Inputs { get; } = inputs;
        public IReadOnlyList<TensorSpec> Outputs { get; } = outputs;

        public IReadOnlyDictionary<string, HostTensor> Run(IReadOnlyList<HostTensor> feed)
        {
            ObjectDisposedException.ThrowIf(_request == 0, this);
            var created = new List<nint>();
            try
            {
                foreach (var t in feed)
                {
                    var spec = Inputs.FirstOrDefault(s => s.Name == t.Name) ?? throw new DeviceFailedException($"unknown input {t.Name}");
                    created.Add(SetInput(t, spec));
                }
                int st = ov_infer_request_infer(_request);
                if (st != OK) throw new DeviceFailedException($"{Device.Label} infer: {LastError(st)}");

                var result = new Dictionary<string, HostTensor>(StringComparer.Ordinal);
                for (int i = 0; i < Outputs.Count; i++)
                {
                    Check(ov_infer_request_get_output_tensor_by_index(_request, (nuint)i, out var tensor), "output");
                    try { result[Outputs[i].Name] = ReadFloat(Outputs[i].Name, tensor); }
                    finally { ov_tensor_free(tensor); }
                }
                return result;
            }
            catch (OpenVinoException e) { throw new DeviceFailedException($"{Device.Label} {e.Message}"); }
            finally { foreach (var t in created) ov_tensor_free(t); }
        }

        private nint SetInput(HostTensor t, TensorSpec spec)
        {
            int et = spec.Type switch
            {
                TensorType.Float32 => ET_F32,
                TensorType.Int64 => ET_I64,
                TensorType.Int32 => ET_I32,
                TensorType.Float16 => ET_F16,
                _ => throw new DeviceFailedException($"input {t.Name} has an unsupported element type"),
            };
            ov_shape_t shape;
            fixed (long* dims = t.Shape) Check(ov_shape_create(t.Shape.Length, dims, &shape), "shape");
            nint tensor;
            try { Check(ov_tensor_create(et, shape, out tensor), "tensor"); }
            finally { ov_shape_free(&shape); }
            Check(ov_tensor_data(tensor, out var data), "tensor data");
            long n = t.Shape.Aggregate(1L, (a, b) => a * b);
            switch (spec.Type)
            {
                case TensorType.Float32:
                    new ReadOnlySpan<float>(Need(t.Floats, n, t.Name)).CopyTo(new Span<float>((void*)data, (int)n)); break;
                case TensorType.Float16:
                {
                    var src = Need(t.Floats, n, t.Name); var dst = new Span<Half>((void*)data, (int)n);
                    for (int i = 0; i < n; i++) dst[i] = (Half)src[i];
                    break;
                }
                case TensorType.Int64:
                    new ReadOnlySpan<long>(Need(t.Longs, n, t.Name)).CopyTo(new Span<long>((void*)data, (int)n)); break;
                case TensorType.Int32:
                {
                    var src = Need(t.Longs, n, t.Name); var dst = new Span<int>((void*)data, (int)n);
                    for (int i = 0; i < n; i++) dst[i] = checked((int)src[i]);
                    break;
                }
            }
            Check(ov_infer_request_set_tensor(_request, t.Name, tensor), $"set input {t.Name}");
            return tensor;
        }

        private static T[] Need<T>(T[]? a, long n, string name)
            => a is not null && a.Length >= n ? a : throw new DeviceFailedException($"input {name} has the wrong size or type");

        private static HostTensor ReadFloat(string name, nint tensor)
        {
            ov_shape_t shape;
            Check(ov_tensor_get_shape(tensor, &shape), "output shape");
            var dims = new long[shape.rank];
            for (int d = 0; d < shape.rank; d++) dims[d] = shape.dims[d];
            ov_shape_free(&shape);
            Check(ov_tensor_get_element_type(tensor, out var et), "output type");
            Check(ov_tensor_get_size(tensor, out var size), "output size");
            Check(ov_tensor_data(tensor, out var data), "output data");
            var f = new float[(int)size];
            switch (et)
            {
                case ET_F32: new ReadOnlySpan<float>((void*)data, f.Length).CopyTo(f); break;
                case ET_F16: { var s = new ReadOnlySpan<Half>((void*)data, f.Length); for (int i = 0; i < f.Length; i++) f[i] = (float)s[i]; break; }
                case ET_BF16: { var s = new ReadOnlySpan<ushort>((void*)data, f.Length); for (int i = 0; i < f.Length; i++) f[i] = BitConverter.Int32BitsToSingle(s[i] << 16); break; }
                default: throw new DeviceFailedException($"output {name} is not floating point");
            }
            return HostTensor.F(name, f, dims);
        }

        public void Dispose()
        {
            if (_request != 0) { ov_infer_request_free(_request); _request = 0; }
            if (_compiled != 0) { ov_compiled_model_free(_compiled); _compiled = 0; }
        }
    }
}
