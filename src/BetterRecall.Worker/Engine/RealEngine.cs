// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Diagnostics;
using System.Security.Cryptography;
using System.Text;
using BetterRecall.Contracts;
using BetterRecall.Worker.Audio;
using BetterRecall.Worker.Hosting;
using BetterRecall.Worker.Inference;
using BetterRecall.Worker.Inference.Onnx;
using BetterRecall.Worker.Inference.OpenVino;
using BetterRecall.Worker.Models;
using BetterRecall.Worker.Protocol;
using BetterRecall.Worker.Vectors;

namespace BetterRecall.Worker.Engine;

/// <summary>Real models. Everything loads lazily on first use; the serial worker loop guarantees one NPU job at a
/// time, so Whisper and the image model never compile or run concurrently.</summary>
public sealed class RealEngine : IWorkerEngine
{
    private readonly WorkerSettings _s;
    private readonly string _home;
    private IInferenceBackend? _main;
    private OnnxBackend? _cpuOrt;
    /// <summary>ONNX Runtime with DirectML for an embedder on a non-Intel graphics card while OpenVINO is the main backend.</summary>
    private OnnxBackend? _gpuOrt;
    private IReadOnlyList<DxgiAdapter>? _adapters;
    private bool? _dml;
    private IReadOnlyList<DeviceInfo>? _gpus;
    private Embedder? _embedder;
    private DeviceChoice? _choice;
    private NsfwGate? _nsfw;
    /// <summary>False in the test seam: the NSFW gate then sees only the injected backends (no DirectML adapter).</summary>
    private readonly bool _probeDml = true;
    private SileroVad? _vad;
    private WhisperGenAi? _whisper;

    public const string BenchText = "BetterRecall benchmark: quarterly sales spreadsheet, Tabelle mit Umsatzzahlen, bảng doanh thu.";

    public RealEngine(WorkerSettings settings, string home) { _s = settings; _home = home; }

    /// <summary>Test seam: inject backends (e.g. CPU only).</summary>
    internal RealEngine(WorkerSettings settings, string home, IInferenceBackend main, OnnxBackend cpu) : this(settings, home)
    {
        _main = main; _cpuOrt = cpu; _probeDml = false;
    }

    private OnnxBackend CpuOrt => _cpuOrt ??= new OnnxBackend(cpuOnly: true);

    /// <summary>DXGI adapters (empty in the test seam).</summary>
    private IReadOnlyList<DxgiAdapter> Adapters => _adapters ??= _probeDml ? DxgiAdapters.Enumerate() : [];

    /// <summary>ONNX Runtime has the DirectML provider (false in the test seam).</summary>
    private bool DmlAvailable
    {
        get
        {
            if (_dml is { } d) return d;
            if (!_probeDml) return (_dml = false).Value;
            try { _dml = Microsoft.ML.OnnxRuntime.OrtEnv.Instance().GetAvailableProviders().Contains("DmlExecutionProvider"); }
            catch (Exception e) when (e is not OutOfMemoryException) { Log.Warn($"ONNX Runtime providers: {e.GetType().Name}"); _dml = false; }
            return _dml.Value;
        }
    }

    /// <summary>Every GPU once (<see cref="GpuSelection.All"/>): OpenVINO GPUs, then DirectML adapters.</summary>
    private IReadOnlyList<DeviceInfo> Gpus
    {
        get
        {
            if (_gpus is not null) return _gpus;
            _gpus = GpuSelection.All(Main.Devices, Adapters, DmlAvailable);
            Log.Info($"GPUs: [{string.Join("; ", _gpus.Select(g => $"{g.Id} {g.Name}"))}]");
            return _gpus;
        }
    }

    /// <summary>The GPU inference.gpuAdapter names (for inference.device = gpu); a setting that matches nothing logs a
    /// warning and gives the automatic GPU.</summary>
    private DeviceInfo? ResolveGpu()
    {
        if (GpuSelection.IsAuto(_s.GpuAdapter)) return GpuSelection.Auto(Main.Devices);
        var gpu = GpuSelection.Resolve(_s.GpuAdapter, Main.Devices, Adapters, DmlAvailable, out var warning);
        if (warning is not null) Log.Warn(warning);
        else Log.Info($"inference.gpuAdapter '{_s.GpuAdapter}' -> {gpu?.Ep} {gpu?.Id} {gpu?.Name}");
        return gpu;
    }

    /// <summary>Backend that runs a device: DirectML devices on ONNX Runtime (a second instance when OpenVINO is the
    /// main backend), everything else on the main backend.</summary>
    private IInferenceBackend BackendFor(DeviceInfo d)
        => d.Ep == "DirectML" && Main is not OnnxBackend ? _gpuOrt ??= new OnnxBackend() : Main;

    private DeviceInfo? FindDevice(string id)
        => Main.Devices.FirstOrDefault(d => d.Id == id) ?? (id.StartsWith("DML:", StringComparison.Ordinal) ? Gpus.FirstOrDefault(d => d.Id == id) : null);

    /// <summary>OpenVINO when its runtime loads and sees an Intel device; otherwise ONNX Runtime.</summary>
    private IInferenceBackend Main
    {
        get
        {
            if (_main is not null) return _main;
            var ov = OpenVinoBackend.TryCreate();
            if (ov is not null && ov.Devices.Any(d => d.Kind != DeviceKind.Cpu || d.Name.Contains("Intel", StringComparison.OrdinalIgnoreCase)))
                return _main = ov;
            ov?.Dispose();
            return _main = new OnnxBackend();
        }
    }

    // ---------------- hello / bench ----------------

    public WorkerHello Hello(int dim)
    {
        var missing = Missing();
        string device = "CPU", ep = Main.Name == "OpenVINO" ? "OpenVINO" : "CPU", model = "google/embeddinggemma-2", rev = "";
        try
        {
            EnsureEmbedder();
            device = _choice!.Device; ep = _choice.Ep;
            model = _embedder!.Manifest.Id; rev = _embedder.Manifest.Revision;
        }
        catch (ModelNotInstalledException) { }
        catch (DeviceFailedException e) { Log.Error($"hello: no usable device: {e.Message}"); }
        bool asr = ModelCatalog.WhisperPresent(_s.ModelDir) && WhisperGenAi.RuntimePresent();
        bool vad = ModelCatalog.VadPath(_s.ModelDir) is not null;
        return new WorkerHello(1, device, ep, model, rev, dim, asr, vad, missing.ToArray(), PreloadNsfw(missing));
    }

    /// <summary>With privacy.nsfwFilter on, loads the NSFW gate now (the detector too when privacy.nsfwDetector is on
    /// and it is installed), so hello reports its device and the first screenshot does not wait for a compile.
    /// Never fails hello: a missing model or a failed device leaves the gate unloaded (null).</summary>
    private string? PreloadNsfw(List<string> missing)
    {
        if (!_s.NsfwFilter || missing.Contains(ModelCatalog.Nsfw)) return null;
        try { return EnsureNsfw(_s.NsfwDetector && !missing.Contains(ModelCatalog.NsfwDetector)).DeviceLabel; }
        catch (WorkerException e) { Log.Warn($"hello: nsfw gate not loaded: {e.Message}"); return null; }
    }

    internal List<string> Missing()
    {
        var m = new List<string>();
        if (ModelCatalog.Load(_s.ModelDir, ModelCatalog.EmbeddingGemma) is null) m.Add(ModelCatalog.EmbeddingGemma);
        if (ModelCatalog.Load(_s.ModelDir, ModelCatalog.Siglip) is null) m.Add(ModelCatalog.Siglip);
        if (!ModelCatalog.WhisperPresent(_s.ModelDir)) m.Add(ModelCatalog.Whisper);
        if (ModelCatalog.VadPath(_s.ModelDir) is null) m.Add(ModelCatalog.Vad);
        if (ModelCatalog.Load(_s.ModelDir, ModelCatalog.Nsfw) is null) m.Add(ModelCatalog.Nsfw);
        if (ModelCatalog.Load(_s.ModelDir, ModelCatalog.NsfwDetector) is null) m.Add(ModelCatalog.NsfwDetector);
        return m;
    }

    public BenchResult Bench()
    {
        _embedder?.Dispose(); _embedder = null; _choice = null;
        return SelectDevice(force: true);
    }

    private void EnsureEmbedder()
    {
        if (_embedder is not null) return;
        SelectDevice(force: false);
    }

    private (ModelManifest? Primary, ModelManifest? Fallback, DeviceCacheKey Key) Models()
    {
        var primary = ModelCatalog.Load(_s.ModelDir, ModelCatalog.EmbeddingGemma);
        var fallback = ModelCatalog.Load(_s.ModelDir, ModelCatalog.Siglip);
        if (primary is null && fallback is null) throw new ModelNotInstalledException(ModelCatalog.EmbeddingGemma);
        var b = Main;
        var key = new DeviceCacheKey(b.Name, b.Version, b.NpuDriverVersion,
            $"{primary?.Hash ?? "none"}+{fallback?.Hash ?? "none"}", DeviceCacheKey.SettingOf(_s.Device, _s.GpuAdapter), _s.ImageTokens);
        return (primary, fallback, key);
    }

    private string CompiledDir(ModelManifest m, DeviceCacheKey key)
        => DeviceCache.CompiledDir(DataPaths.CompiledCache(_home), m.Name,
            new DeviceCacheKey(key.Runtime, key.RuntimeVersion, key.NpuDriver, m.Hash, "", 0).CompiledId);

    /// <summary>Uses cache\device.json when its key matches; otherwise benchmarks and rewrites it.</summary>
    private BenchResult SelectDevice(bool force)
    {
        var (primary, fallback, key) = Models();
        var cachePath = DataPaths.DeviceCache(_home);
        // inference.gpuAdapter is resolved against today's adapters (a DXGI index can move when cards change).
        var gpu = _s.Device == "gpu" ? ResolveGpu() : null;
        if (!force && DeviceCache.Read(cachePath, key) is { } cached)
        {
            var dev = FindDevice(cached.Chosen!.DeviceId);
            var textDev = cached.Chosen!.TextDeviceId is { } tid ? FindDevice(tid) : dev;
            var m = cached.Chosen!.Model == ModelCatalog.Siglip ? fallback : primary;
            if (dev is { Kind: DeviceKind.Gpu } && gpu is not null && dev.Id != gpu.Id) dev = null;
            if (dev is not null && textDev is not null && m is not null)
            {
                try
                {
                    var sw = Stopwatch.StartNew();
                    var e = new Embedder(BackendFor(dev), dev, m, _s.ImageTokens, CompiledDir(m, key), textDev);
                    e.LoadAll();
                    _embedder = e; _choice = cached.Chosen;
                    Log.Info($"device from cache: {_choice.Device} {_choice.Ep} {_choice.Model}, load {sw.ElapsedMilliseconds} ms");
                    return cached.Bench ?? new BenchResult(_choice.Device, []);
                }
                catch (Exception e) when (e is DeviceFailedException or InvalidDataException or IOException)
                {
                    Log.Warn($"cached device failed to load ({e.Message}); benchmarking again");
                }
            }
        }

        var outcomes = new List<BenchOutcome>();
        var keep = new Dictionary<BenchOutcome, Embedder>();
        // Per-budget image timings only for the bench op (they compile every budget: slow on a first NPU run).
        var tokens = force ? new List<TokenRow>() : null;
        var allowed = DevicePolicy.Allowed(_s.Device, Main.Devices, gpu);
        // ONNX Runtime main backend: its DirectML device takes the adapter's name from the GPU list.
        if (allowed.Any(d => d.Ep == "DirectML")) allowed = allowed.Select(d => Gpus.FirstOrDefault(g => g.Id == d.Id) ?? d).ToList();
        var allowedIds = allowed.Select(d => d.Id).ToHashSet(StringComparer.Ordinal);
        // The bench op also measures every other GPU when there is more than one (measure-only rows, never chosen).
        var measured = force ? DevicePolicy.BenchDevices(allowed, Gpus) : allowed;
        if (primary is not null) BenchModel(primary, measured, key, outcomes, keep, tokens, allowedIds);
        bool npuPrimaryFailed = outcomes.Any(o => o.Model == ModelCatalog.EmbeddingGemma && o.Device.Kind == DeviceKind.Npu && !o.Ok);
        // Measure-only GPU rows do not count: the fallback is for when nothing the setting allows passed.
        bool primaryAllFailed = primary is null || !outcomes.Any(o => o.Model == ModelCatalog.EmbeddingGemma && o.Ok && allowedIds.Contains(o.Device.Id));
        if (fallback is not null && (npuPrimaryFailed || primaryAllFailed))
        {
            var fbDevices = npuPrimaryFailed ? allowed.Where(d => d.Kind is DeviceKind.Npu or DeviceKind.Cpu).ToList() : allowed.ToList();
            BenchModel(fallback, fbDevices, key, outcomes, keep, tokens, allowedIds);
        }

        var chosen = DevicePolicy.Choose(outcomes.Where(o => allowedIds.Contains(o.Device.Id)).ToList(), ModelCatalog.EmbeddingGemma, ModelCatalog.Siglip);
        foreach (var (o, e) in keep) if (!ReferenceEquals(o, chosen)) e.Dispose();
        // Image-token budgets only on the chosen device: other budgets compile extra graphs (slow on a first NPU run).
        if (tokens is not null && chosen is not null) BenchBudgets(keep[chosen], chosen, tokens);
        var tokenRows = tokens?.Where(t => chosen is not null && t.Model == chosen.Model)
                              .Select(t => t.Row).ToArray();
        // GPU rows carry the adapter name when the PC has more than one GPU.
        var gpus = outcomes.Any(o => o.Device.Kind == DeviceKind.Gpu) ? Gpus : [];
        var autoGpu = GpuSelection.Auto(Main.Devices);
        var result = new BenchResult(chosen?.Device.Label ?? "none", outcomes.Select(o => ToBenchDevice(o, gpus, autoGpu)).ToArray(),
            tokenRows is { Length: > 0 } ? tokenRows : null,
            chosen is { Device.Kind: DeviceKind.Gpu } && gpus.Count > 1 ? chosen.Device.Name : null);
        if (chosen is null)
        {
            var why = outcomes.FirstOrDefault(o => o.Error is not null)?.Error ?? "no device passed";
            throw new DeviceFailedException(why);
        }
        _embedder = keep[chosen];
        _choice = new DeviceChoice(chosen.Device.Label, chosen.Device.Id, chosen.Device.Ep, chosen.Model, chosen.Split ? chosen.TextDevice!.Id : null);
        DeviceCache.Write(cachePath, new DeviceCacheFile { Key = key, Chosen = _choice, Bench = result, WrittenUtc = DateTimeOffset.UtcNow });
        Log.Info($"device chosen: {_choice.Device} ({_choice.DeviceId} {chosen.Device.Name}) {_choice.Model}{(_choice.TextDeviceId is { } t ? " text on " + t : "")}");
        return result;
    }

    /// <summary>gpus: every GPU of the PC; with more than one, GPU rows carry the adapter name (Adapter) and whether
    /// it is the one inference.gpuAdapter "auto" picks (AdapterAuto, by device id against autoGpu).</summary>
    internal static BenchDevice ToBenchDevice(BenchOutcome o, IReadOnlyList<DeviceInfo>? gpus = null, DeviceInfo? autoGpu = null)
    {
        bool named = o.Device.Kind == DeviceKind.Gpu && gpus is { Count: > 1 };
        return new(o.Device.Label, o.Device.Ep, o.Ok, Math.Round(o.ImageMs, 1), Math.Round(o.TextMs, 1),
            double.IsNaN(o.Cosine) ? 0 : Math.Round(o.Cosine, 6), o.Error,
            Model: o.Model,
            TextDevice: o.Split ? o.TextDevice!.Label : null,
            CompileMs: o.CompileCached || o.CompileMs <= 0 ? null : Math.Round(o.CompileMs),
            Adapter: named ? o.Device.Name : null,
            AdapterAuto: named && autoGpu is not null && autoGpu.Id == o.Device.Id);
    }

    /// <summary>One TokenBench row and the model it was measured with (rows of the chosen model are reported).</summary>
    private sealed record TokenRow(string Model, TokenBench Row);

    /// <summary>CPU reference first, then every other device: compile, warm up, time, compare by cosine. Only devices
    /// in keepIds (the allowed ones) keep their loaded embedder for the choice; the others are measured and freed.</summary>
    private void BenchModel(ModelManifest m, IReadOnlyList<DeviceInfo> devices, DeviceCacheKey key,
        List<BenchOutcome> outcomes, Dictionary<BenchOutcome, Embedder> keep, List<TokenRow>? tokens, IReadOnlySet<string> keepIds)
    {
        var image = BenchImage();
        float[]? refImg = null, refTxt = null;
        var order = devices.Where(d => d.Kind == DeviceKind.Cpu).Concat(devices.Where(d => d.Kind != DeviceKind.Cpu));
        var cpu = devices.FirstOrDefault(d => d.Kind == DeviceKind.Cpu);
        foreach (var dev in order)
        {
            var k = keepIds.Contains(dev.Id) ? keep : null;
            var o = BenchOne(m, dev, dev, key, image, ref refImg, ref refTxt, k, tokens);
            outcomes.Add(o);
            // Split: the vision tower passed on the accelerator but the whole model did not. Try text on the CPU
            // (same backend only: one embedder runs on one backend).
            if (!o.Ok && dev.Kind != DeviceKind.Cpu && cpu is not null && refImg is not null && m.Vision.Any(v => v.Fusion?.InputsEmbeds is not null)
                && ReferenceEquals(BackendFor(dev), BackendFor(cpu)))
                outcomes.Add(BenchOne(m, dev, cpu, key, image, ref refImg, ref refTxt, k, tokens));
        }
    }

    private BenchOutcome BenchOne(ModelManifest m, DeviceInfo dev, DeviceInfo textDev, DeviceCacheKey key, RgbImage image,
        ref float[]? refImg, ref float[]? refTxt, Dictionary<BenchOutcome, Embedder>? keep, List<TokenRow>? tokens)
    {
        Embedder? e = null;
        string where = dev.Id == textDev.Id ? dev.Id : $"{dev.Id}+text:{textDev.Id}";
        try
        {
            var dir = CompiledDir(m, key);
            int blobsBefore = CountFiles(dir);
            var sw = Stopwatch.StartNew();
            var backend = BackendFor(dev);
            e = new Embedder(backend, dev, m, _s.ImageTokens, dir, textDev);
            e.LoadAll();
            double compileMs = sw.Elapsed.TotalMilliseconds;
            // Cache hit: OpenVINO wrote no new blob (ONNX Runtime has no compiled cache, so it always compiles).
            bool cached = backend is OpenVinoBackend && CountFiles(dir) == blobsBefore;
            // The CPU is steady and slow: one timed run after warm-up. Accelerators: median of three.
            int runs = dev.Kind == DeviceKind.Cpu ? 1 : 3;
            var (img, imgMs) = Time(() => e.EmbedImage(image), runs);
            var (txt, txtMs) = Time(() => e.EmbedText(BenchText, TextKind.Query), dev.Kind == textDev.Kind ? runs : 1);
            bool nan = VectorMath.HasNonFinite(img) || VectorMath.HasNonFinite(txt);
            double cos;
            if (dev.Kind == DeviceKind.Cpu && textDev.Kind == DeviceKind.Cpu)
            {
                if (nan) throw new DeviceFailedException("CPU reference output contains NaN");
                refImg = img; refTxt = txt; cos = 1.0;
            }
            else if (refImg is null || refTxt is null) throw new DeviceFailedException("no CPU reference");
            else cos = Math.Min(VectorMath.Cosine(img, refImg), VectorMath.Cosine(txt, refTxt));
            bool ok = DevicePolicy.Accept(cos, nan);
            var o = new BenchOutcome(dev, m.Name, ok, imgMs, txtMs, cos,
                ok ? null : nan ? "output contains NaN" : $"cosine {cos:F4} below {DevicePolicy.MinCosine}", compileMs, textDev)
                { CompileCached = cached };
            Log.Info($"bench {m.Name} {where}: ok={ok} compile={compileMs:F0}ms image={imgMs:F1}ms text={txtMs:F1}ms cos={cos:F6}" +
                     (e.DroppedLengths.Count > 0 ? $" dropped lengths [{string.Join(',', e.DroppedLengths)}]" : ""));
            if (ok && keep is not null) { keep[o] = e; e = null; }
            return o;
        }
        catch (Exception ex) when (ex is DeviceFailedException or InvalidDataException or IOException)
        {
            var msg = ex is DeviceFailedException ? ex.Message["Device failed: ".Length..].TrimEnd('.') : ex.Message;
            Log.Warn($"bench {m.Name} {where} failed: {msg}");
            return new BenchOutcome(dev, m.Name, false, 0, 0, double.NaN, msg, 0, textDev);
        }
        finally { e?.Dispose(); }
    }

    private void BenchBudgets(Embedder e, BenchOutcome o, List<TokenRow> tokens)
    {
        var image = BenchImage();
        int runs = o.Device.Kind == DeviceKind.Cpu ? 1 : 3;
        foreach (int b in e.Budgets)
        {
            if (b == e.ImageTokens)
            {
                tokens.Add(new TokenRow(o.Model, new TokenBench(o.Device.Label, b, Math.Round(o.ImageMs, 1))));
                continue;
            }
            try
            {
                var sw = Stopwatch.StartNew();
                var (_, ms) = Time(() => e.EmbedImage(image, b), runs);
                tokens.Add(new TokenRow(o.Model, new TokenBench(o.Device.Label, b, Math.Round(ms, 1))));
                Log.Info($"bench {o.Model} {o.Device.Id} tokens={b}: image={ms:F1}ms (with compile {sw.ElapsedMilliseconds} ms)");
            }
            catch (DeviceFailedException ex)
            {
                Log.Warn($"bench {o.Model} {o.Device.Id} tokens={b} failed: {ex.Message}");
            }
        }
    }

    private static int CountFiles(string dir)
    {
        try { return Directory.Exists(dir) ? Directory.GetFiles(dir, "*", SearchOption.AllDirectories).Length : 0; }
        catch (Exception e) when (e is IOException or UnauthorizedAccessException) { return 0; }
    }

    /// <summary>Warm-up once, then the median of the timed runs.</summary>
    private static (float[] V, double Ms) Time(Func<float[]> f, int runs = 3)
    {
        var v = f();
        var t = new double[Math.Max(1, runs)];
        for (int i = 0; i < t.Length; i++) { var sw = Stopwatch.StartNew(); v = f(); t[i] = sw.Elapsed.TotalMilliseconds; }
        Array.Sort(t);
        return (v, t[t.Length / 2]);
    }

    /// <summary>Deterministic 1280x720 test frame: gradients, a grid and blocks, like a busy desktop.</summary>
    internal static RgbImage BenchImage()
    {
        const int w = 1280, h = 720;
        var rgb = new byte[w * h * 3];
        for (int y = 0; y < h; y++)
            for (int x = 0; x < w; x++)
            {
                int o = (y * w + x) * 3;
                bool grid = x % 64 == 0 || y % 48 == 0;
                bool block = (x / 160 + y / 120) % 3 == 0;
                rgb[o] = (byte)(grid ? 30 : block ? 220 : x * 255 / w);
                rgb[o + 1] = (byte)(grid ? 30 : block ? 230 : y * 255 / h);
                rgb[o + 2] = (byte)(grid ? 30 : block ? 240 : (x + y) % 256);
            }
        return new RgbImage(w, h, rgb);
    }

    // ---------------- ops ----------------

    public ImageEmbedding EmbedImage(ImageInput image, double? nsfwThreshold, int nsfwGrid = 1, double? nsfwDetector = null)
    {
        var rgb = ImagePipeline.Decode(image);
        if (nsfwThreshold is not null || nsfwDetector is not null)
        {
            var gate = EnsureNsfw(nsfwDetector is not null);
            // Classifier first (cheap, stops at the first blocking tile); the detector only for frames it passed.
            if (nsfwThreshold is { } t && gate.Classifier.Score(rgb, t, nsfwGrid) >= t) return new ImageEmbedding(null, "nsfw");
            if (nsfwDetector is { } dt && gate.Detector!.Score(rgb) >= dt) return new ImageEmbedding(null, "nsfw");
        }
        EnsureEmbedder();
        return new ImageEmbedding(_embedder!.EmbedImage(rgb), null);
    }

    /// <summary>The loaded gate, or a new one on the first device of <see cref="NsfwCandidates"/> that loads both
    /// models and passes a warm-up. A gate loaded without the detector is replaced when a request needs it.</summary>
    private NsfwGate EnsureNsfw(bool detector)
    {
        if (_nsfw is not null && (!detector || _nsfw.Detector is not null)) return _nsfw;
        var cls = ModelCatalog.Load(_s.ModelDir, ModelCatalog.Nsfw) ?? throw new ModelNotInstalledException(ModelCatalog.Nsfw);
        var det = detector ? ModelCatalog.Load(_s.ModelDir, ModelCatalog.NsfwDetector) ?? throw new ModelNotInstalledException(ModelCatalog.NsfwDetector) : null;
        _nsfw?.Dispose();
        _nsfw = null;
        return _nsfw = NsfwGate.Load(NsfwCandidates(cls, det ?? ModelCatalog.Load(_s.ModelDir, ModelCatalog.NsfwDetector)), cls, det);
    }

    /// <summary>privacy.nsfwDevice order over: OpenVINO NPU and integrated GPU (when the main backend is OpenVINO),
    /// the DirectML adapter picked by privacy.nsfwGpuAdapter, and the ONNX Runtime CPU (INT8 graph). OpenVINO devices
    /// share cache\compiled\nsfw-&lt;id&gt;, keyed by runtime, driver and both manifests.</summary>
    private List<NsfwCandidate> NsfwCandidates(ModelManifest cls, ModelManifest? det)
    {
        var available = new List<DeviceInfo>();
        var backends = new Dictionary<DeviceInfo, IInferenceBackend>();
        bool wantsOv = _s.NsfwDevice is "auto" or "npu" or "igpu";
        if (wantsOv && Main is OpenVinoBackend ov)
            foreach (var d in ov.Devices.Where(d => d.Kind != DeviceKind.Cpu)) { available.Add(d); backends[d] = ov; }
        if (_probeDml && _s.NsfwDevice is "auto" or "gpu" && NsfwDmlDevice() is { } dml) { available.Add(dml); backends[dml] = CpuOrt; }
        var cpu = CpuOrt.Devices.First(d => d.Kind == DeviceKind.Cpu);
        available.Add(cpu); backends[cpu] = CpuOrt;

        string? ovCache = null;
        var order = NsfwDevicePlan.Order(_s.NsfwDevice, available);
        if (order.Any(d => d.Ep == "OpenVINO"))
            ovCache = DeviceCache.CompiledDir(DataPaths.CompiledCache(_home), "nsfw",
                new DeviceCacheKey(Main.Name, Main.Version, Main.NpuDriverVersion, $"{cls.Hash}+{det?.Hash ?? "none"}", "", 0).CompiledId);
        Log.Info($"nsfw device order ({_s.NsfwDevice}): {string.Join(", ", order.Select(d => $"{NsfwDevicePlan.Label(d)} {d.Ep} {d.Id}"))}");
        return order.Select(d => new NsfwCandidate(d, backends[d], d.Ep == "OpenVINO" ? ovCache : null)).ToList();
    }

    /// <summary>DirectML device on the DXGI adapter chosen by privacy.nsfwGpuAdapter, or null (no DirectML provider,
    /// no DXGI, or no matching adapter).</summary>
    private DeviceInfo? NsfwDmlDevice()
    {
        try
        {
            if (!Microsoft.ML.OnnxRuntime.OrtEnv.Instance().GetAvailableProviders().Contains("DmlExecutionProvider")) return null;
        }
        catch (Exception e) when (e is not OutOfMemoryException) { Log.Warn($"nsfw: ONNX Runtime providers: {e.GetType().Name}"); return null; }
        var adapters = DxgiAdapters.Enumerate();
        var pick = DxgiAdapters.Pick(_s.NsfwGpuAdapter, adapters);
        Log.Info($"nsfw: DXGI adapters [{string.Join("; ", adapters.Select(a => $"{a.Index} {a.Description} {a.DedicatedVideoMemory >> 20} MB"))}], " +
                 $"setting '{_s.NsfwGpuAdapter}' -> {(pick is null ? "none" : pick.Index + " " + pick.Description)}");
        return pick is null ? null : new DeviceInfo(DeviceKind.Gpu, $"DML:{pick.Index}", pick.Description, "DirectML");
    }

    public float[] EmbedText(string text, TextKind kind)
    {
        EnsureEmbedder();
        return _embedder!.EmbedText(text, kind);
    }

    public TranscribeResult Transcribe(float[] pcm, string source, string language)
    {
        long started = Stopwatch.GetTimestamp();
        if (_vad is null)
        {
            var p = ModelCatalog.VadPath(_s.ModelDir) ?? throw new ModelNotInstalledException(ModelCatalog.Vad);
            _vad = new SileroVad(CpuOrt, p);
        }
        var segs = _vad.Segments(pcm);
        if (segs.Count == 0)
            return new TranscribeResult(false, "", language == "auto" ? "und" : language, [], (long)Stopwatch.GetElapsedTime(started).TotalMilliseconds);
        _whisper ??= LoadWhisper();
        return _whisper.Transcribe(pcm, language, started);
    }

    /// <summary>NPU then CPU for auto/npu; the Intel GPU then CPU for gpu; CPU for cpu.</summary>
    private WhisperGenAi LoadWhisper()
    {
        if (!ModelCatalog.WhisperPresent(_s.ModelDir)) throw new ModelNotInstalledException(ModelCatalog.Whisper);
        if (!WhisperGenAi.RuntimePresent()) throw new DeviceFailedException("OpenVINO GenAI runtime not found");
        var ovDevices = (Main as OpenVinoBackend)?.Devices ?? [new DeviceInfo(DeviceKind.Cpu, "CPU", "CPU", "OpenVINO")];
        var order = DevicePolicy.Allowed(_s.Device, ovDevices);
        var dir = ModelCatalog.Dir(_s.ModelDir, ModelCatalog.Whisper);
        var cache = DeviceCache.CompiledDir(DataPaths.CompiledCache(_home), ModelCatalog.Whisper,
            new DeviceCacheKey("OpenVINO GenAI", Main.Version, Main.NpuDriverVersion, FolderHash(dir), "", 0).CompiledId);
        DeviceFailedException? last = null;
        foreach (var d in order)
        {
            try
            {
                var sw = Stopwatch.StartNew();
                var w = new WhisperGenAi(dir, d.Id, cache);
                Log.Info($"whisper loaded on {d.Id} in {sw.ElapsedMilliseconds} ms");
                return w;
            }
            catch (DeviceFailedException e) { last = e; Log.Warn($"whisper on {d.Id}: {e.Message}"); }
        }
        throw last ?? new DeviceFailedException("Whisper failed on every device");
    }

    /// <summary>Names and sizes of the files in a model folder: a cheap stand-in for a content hash.</summary>
    internal static string FolderHash(string dir)
    {
        var sb = new StringBuilder();
        foreach (var f in Directory.GetFiles(dir).OrderBy(f => f, StringComparer.OrdinalIgnoreCase))
            sb.Append(Path.GetFileName(f)).Append(':').Append(new FileInfo(f).Length).Append('|');
        return Convert.ToHexString(SHA256.HashData(Encoding.UTF8.GetBytes(sb.ToString()))).ToLowerInvariant();
    }

    public void Dispose()
    {
        _embedder?.Dispose();
        _nsfw?.Dispose();
        _vad?.Dispose();
        _whisper?.Dispose();
        _main?.Dispose();
        _cpuOrt?.Dispose();
        _gpuOrt?.Dispose();
    }
}
