// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Diagnostics;
using BetterRecall.Worker.Hosting;
using BetterRecall.Worker.Inference;
using BetterRecall.Worker.Protocol;

namespace BetterRecall.Worker.Models;

/// <summary>One NSFW gate candidate: a device and the backend that compiles for it, plus its compiled-model cache
/// directory (OpenVINO only; null for ONNX Runtime).</summary>
public sealed record NsfwCandidate(DeviceInfo Device, IInferenceBackend Backend, string? CacheDir);

/// <summary>The NSFW classifier and (optionally) detector, both on the same device.</summary>
public sealed class NsfwGate : IDisposable
{
    public DeviceInfo Device { get; }
    public string DeviceLabel => NsfwDevicePlan.Label(Device);
    public NsfwClassifier Classifier { get; }
    public NsfwDetector? Detector { get; }

    private NsfwGate(DeviceInfo device, NsfwClassifier classifier, NsfwDetector? detector)
    {
        Device = device; Classifier = classifier; Detector = detector;
    }

    /// <summary>Tries the candidates in order (see <see cref="NsfwDevicePlan.Order"/>): loads the classifier and the
    /// detector (when given) and runs one warm-up inference each on a zero frame. Any failure disposes what loaded,
    /// is logged, and moves to the next candidate. Throws DeviceFailedException when every candidate failed.</summary>
    public static NsfwGate Load(IReadOnlyList<NsfwCandidate> candidates, ModelManifest classifier, ModelManifest? detector)
    {
        string? last = null;
        foreach (var c in candidates)
        {
            NsfwClassifier? cls = null;
            NsfwDetector? det = null;
            var sw = Stopwatch.StartNew();
            try
            {
                cls = new NsfwClassifier(c.Backend, c.Device, classifier, c.CacheDir);
                cls.WarmUp();
                if (detector is not null)
                {
                    det = new NsfwDetector(c.Backend, c.Device, detector, c.CacheDir);
                    det.WarmUp();
                }
                var gate = new NsfwGate(c.Device, cls, det);
                Log.Info($"nsfw gate on {gate.DeviceLabel} ({c.Device.Ep} {c.Device.Id}): classifier{(det is not null ? " + detector" : "")}, load {sw.ElapsedMilliseconds} ms");
                return gate;
            }
            catch (Exception e) when (e is not OutOfMemoryException)
            {
                det?.Dispose();
                cls?.Dispose();
                const string prefix = "Device failed: ";
                last = e is DeviceFailedException && e.Message.StartsWith(prefix, StringComparison.Ordinal)
                    ? e.Message[prefix.Length..].TrimEnd('.') : $"{c.Device.Label} {e.GetType().Name}: {e.Message}";
                Log.Warn($"nsfw gate on {NsfwDevicePlan.Label(c.Device)} ({c.Device.Ep} {c.Device.Id}) failed: {last}");
            }
        }
        throw new DeviceFailedException(last is null ? "nsfw gate has no device" : $"nsfw gate failed on every device ({last})");
    }

    public void Dispose()
    {
        Detector?.Dispose();
        Classifier.Dispose();
    }
}
