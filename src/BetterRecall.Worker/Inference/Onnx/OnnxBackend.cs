// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using BetterRecall.Worker.Hosting;
using BetterRecall.Worker.Protocol;
using Microsoft.ML.OnnxRuntime;
using Microsoft.ML.OnnxRuntime.Tensors;

namespace BetterRecall.Worker.Inference.Onnx;

/// <summary>ONNX Runtime: CPU always, QNN (NPU) when the QNN provider is present, DirectML (GPU) otherwise.
/// Used for the embedder on non-Intel machines, for Silero VAD, and for the NSFW gate on the CPU or a DirectML graphics card.</summary>
public sealed class OnnxBackend : IInferenceBackend
{
    public string Name => "ONNX Runtime";
    public string Version { get; }
    public string NpuDriverVersion => "";
    public IReadOnlyList<DeviceInfo> Devices { get; }

    /// <summary>cpuOnly: VAD and the CPU slot of the NSFW gate never touch a GPU or NPU.</summary>
    public OnnxBackend(bool cpuOnly = false)
    {
        var env = OrtEnv.Instance();
        Version = env.GetVersionString();
        var providers = env.GetAvailableProviders();
        var devices = new List<DeviceInfo>();
        if (!cpuOnly)
        {
            if (providers.Contains("QNNExecutionProvider")) devices.Add(new DeviceInfo(DeviceKind.Npu, "QNN", "Qualcomm NPU (QNN HTP)", "QNN"));
            if (providers.Contains("DmlExecutionProvider")) devices.Add(new DeviceInfo(DeviceKind.Gpu, "DML:0", "DirectML default adapter", "DirectML"));
        }
        devices.Add(new DeviceInfo(DeviceKind.Cpu, "CPU", "CPU", "CPU"));
        Devices = devices;
        Log.Debug($"ONNX Runtime {Version}: providers [{string.Join(",", providers)}]");
    }

    public static SessionOptions CpuOptions(int threads)
    {
        var o = new SessionOptions
        {
            IntraOpNumThreads = threads,
            InterOpNumThreads = 1,
            ExecutionMode = ExecutionMode.ORT_SEQUENTIAL,
            GraphOptimizationLevel = GraphOptimizationLevel.ORT_ENABLE_ALL,
        };
        o.AddSessionConfigEntry("session.intra_op.allow_spinning", "0");
        return o;
    }

    /// <summary>staticShapes is ignored: CPU and DirectML run dynamic ONNX graphs as they are. QNN needs graphs
    /// exported with static shapes. OpenVINO IR (.xml) fails fast with a clear message: ONNX Runtime reads only ONNX.</summary>
    public IInferenceSession Load(string modelPath, DeviceInfo device, string? cacheDir, IReadOnlyDictionary<string, long[]>? staticShapes = null)
    {
        if (modelPath.EndsWith(".xml", StringComparison.OrdinalIgnoreCase))
            throw new DeviceFailedException($"{device.Label} ONNX Runtime cannot read OpenVINO IR ({Path.GetFileName(modelPath)}); the model has no ONNX graph");
        SessionOptions? o = null;
        try
        {
            o = CpuOptions(Math.Clamp(Environment.ProcessorCount / 4, 2, 4));
            switch (device.Ep)
            {
                case "QNN":
                    o.AppendExecutionProvider("QNN", new Dictionary<string, string>
                    {
                        ["backend_path"] = "QnnHtp.dll",
                        ["htp_performance_mode"] = "low_power_saver",
                    });
                    o.AddSessionConfigEntry("session.disable_cpu_ep_fallback", "1");
                    break;
                case "DirectML":
                    o.EnableMemoryPattern = false;
                    o.AppendExecutionProvider_DML(DmlDeviceId(device.Id));
                    break;
            }
            var session = new InferenceSession(modelPath, o);
            return new OrtSession(device, session, o);
        }
        catch (OnnxRuntimeException e)
        {
            o?.Dispose();
            throw new DeviceFailedException($"{device.Label} {FirstLine(e.Message)}");
        }
        catch
        {
            o?.Dispose();
            throw;
        }
    }

    /// <summary>"DML:&lt;n&gt;" -> n (the DXGI EnumAdapters1 index DirectML takes as device_id); 0 for anything else.</summary>
    internal static int DmlDeviceId(string id)
        => id.StartsWith("DML:", StringComparison.Ordinal) && int.TryParse(id.AsSpan(4), System.Globalization.NumberStyles.None,
               System.Globalization.CultureInfo.InvariantCulture, out var n) ? n : 0;

    private static string FirstLine(string s) => s.Split('\n')[0].Trim();

    public void Dispose() { }

    private sealed class OrtSession : IInferenceSession
    {
        private readonly InferenceSession _session;
        private readonly SessionOptions _options;
        public DeviceInfo Device { get; }
        public IReadOnlyList<TensorSpec> Inputs { get; }
        public IReadOnlyList<TensorSpec> Outputs { get; }

        public OrtSession(DeviceInfo device, InferenceSession session, SessionOptions options)
        {
            Device = device; _session = session; _options = options;
            Inputs = session.InputMetadata.Select(kv => Spec(kv.Key, kv.Value)).ToList();
            Outputs = session.OutputMetadata.Select(kv => Spec(kv.Key, kv.Value)).ToList();
        }

        private static TensorSpec Spec(string name, NodeMetadata m)
        {
            var t = m.IsTensor ? m.ElementDataType switch
            {
                TensorElementType.Float => TensorType.Float32,
                TensorElementType.Int64 => TensorType.Int64,
                TensorElementType.Int32 => TensorType.Int32,
                TensorElementType.Float16 => TensorType.Float16,
                _ => TensorType.Other,
            } : TensorType.Other;
            return new TensorSpec(name, t, m.IsTensor ? m.Dimensions.Select(d => (long)d).ToArray() : []);
        }

        public IReadOnlyDictionary<string, HostTensor> Run(IReadOnlyList<HostTensor> feed)
        {
            var inputs = new List<NamedOnnxValue>(feed.Count);
            foreach (var t in feed)
            {
                var spec = Inputs.FirstOrDefault(s => s.Name == t.Name) ?? throw new DeviceFailedException($"unknown input {t.Name}");
                var dims = t.Shape.Select(d => checked((int)d)).ToArray();
                inputs.Add(spec.Type switch
                {
                    TensorType.Float32 => NamedOnnxValue.CreateFromTensor(t.Name, new DenseTensor<float>(t.Floats!, dims)),
                    TensorType.Float16 => NamedOnnxValue.CreateFromTensor(t.Name, new DenseTensor<Float16>(t.Floats!.Select(x => (Float16)x).ToArray(), dims)),
                    TensorType.Int64 => NamedOnnxValue.CreateFromTensor(t.Name, new DenseTensor<long>(t.Longs!, dims)),
                    TensorType.Int32 => NamedOnnxValue.CreateFromTensor(t.Name, new DenseTensor<int>(t.Longs!.Select(x => checked((int)x)).ToArray(), dims)),
                    _ => throw new DeviceFailedException($"input {t.Name} has an unsupported element type"),
                });
            }
            try
            {
                using var results = _session.Run(inputs);
                var map = new Dictionary<string, HostTensor>(StringComparer.Ordinal);
                foreach (var r in results)
                {
                    if (r.Value is Tensor<float> f) map[r.Name] = HostTensor.F(r.Name, f.ToArray(), f.Dimensions.ToArray().Select(d => (long)d).ToArray());
                    else if (r.Value is Tensor<Float16> h) map[r.Name] = HostTensor.F(r.Name, h.ToArray().Select(x => (float)x).ToArray(), h.Dimensions.ToArray().Select(d => (long)d).ToArray());
                }
                return map;
            }
            catch (OnnxRuntimeException e) { throw new DeviceFailedException($"{Device.Label} {FirstLine(e.Message)}"); }
        }

        public void Dispose() { _session.Dispose(); _options.Dispose(); }
    }
}
