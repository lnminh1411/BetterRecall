// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Buffers.Binary;
using System.IO.Pipes;
using System.Text;
using System.Text.Json;
using BetterRecall.Contracts;
using BetterRecall.Worker.Engine;
using BetterRecall.Worker.Hosting;
using BetterRecall.Worker.Inference;
using BetterRecall.Worker.Inference.Onnx;
using BetterRecall.Worker.Protocol;

namespace BetterRecall.Worker.Tests;

/// <summary>Bug sweep 2, batch G: worker fixes (C3, C13, C19, D27).</summary>
public class Sweep2GTests
{
    // ---------------- C3: idle timer mid-frame ----------------

    [Fact]
    public async Task Idle_timer_firing_mid_frame_finishes_the_frame()
    {
        var home = TestEnv.NewDir("idle");
        var s = TestEnv.Settings(home, x => x.Inference.Fake = true);
        var name = PipeNames.NewWorker();
        // 0-byte buffers like the agent's worker pipe: every write waits for the reader.
        await using var server = new NamedPipeServerStream(name, PipeDirection.InOut, 1, PipeTransmissionMode.Byte, PipeOptions.Asynchronous, 0, 0);
        var wait = server.WaitForConnectionAsync();
        await using var client = await AgentPipe.ConnectAsync(name, TimeSpan.FromSeconds(5), CancellationToken.None);
        await wait;
        var handler = new RequestHandler(new FakeEngine(), () => s);
        var loop = Task.Run(() => WorkerLoop.RunAsync(client, handler, TimeSpan.FromMilliseconds(300), CancellationToken.None,
            () => { try { return AgentPipe.HasPendingInput(client.SafePipeHandle); } catch { return false; } }));

        var header = JsonSerializer.SerializeToUtf8Bytes(new WorkerRequest(5, "hello"), ContractsJson.Default.WorkerRequest);
        var len = new byte[4];
        BinaryPrimitives.WriteUInt32LittleEndian(len, (uint)header.Length);
        await server.WriteAsync(len);
        await server.FlushAsync();
        await Task.Delay(800);   // the idle timer fires while the worker holds the length but not the header
        await server.WriteAsync(header);
        BinaryPrimitives.WriteUInt32LittleEndian(len, 0);
        await server.WriteAsync(len);
        await server.FlushAsync();

        using var cts = new CancellationTokenSource(TimeSpan.FromSeconds(10));
        var reply = await BinaryFrame.ReadAsync(server, cts.Token);
        Assert.NotNull(reply);
        var r = JsonSerializer.Deserialize(reply.Value.Header, ContractsJson.Default.WorkerResponse)!;
        Assert.True(r.Ok, r.Error);
        Assert.Equal(5, r.Id);
        // Then a clean idle exit, not a protocol error.
        Assert.Equal(LoopExit.Idle, await loop.WaitAsync(TimeSpan.FromSeconds(5)));
    }

    [Fact]
    public async Task Frame_write_and_read_round_trip_with_start_token()
    {
        using var ms = new MemoryStream();
        await BinaryFrame.WriteAsync(ms, "{\"id\":1}"u8.ToArray(), new byte[] { 1, 2, 3 }, CancellationToken.None);
        Assert.Equal(4 + 8 + 4 + 3, ms.Length);
        ms.Position = 0;
        using var start = new CancellationTokenSource();
        var f = await BinaryFrame.ReadAsync(ms, CancellationToken.None, start.Token);
        Assert.Equal("{\"id\":1}", Encoding.UTF8.GetString(f!.Value.Header));
        Assert.Equal(new byte[] { 1, 2, 3 }, f.Value.Payload);
        Assert.Null(await BinaryFrame.ReadAsync(ms, CancellationToken.None, start.Token));   // clean end between frames
    }

    // ---------------- C19: Whisper language list ----------------

    [Theory]
    [InlineData("auto", true)]
    [InlineData("en", true)]
    [InlineData("vi", true)]
    [InlineData("haw", true)]
    [InlineData("yue", true)]
    [InlineData("xx", false)]
    [InlineData("zz", false)]
    [InlineData("eng", false)]
    [InlineData("EN", false)]
    public void Language_must_be_a_whisper_code(string lang, bool ok)
        => Assert.Equal(ok, RequestHandler.IsLanguage(lang));

    [Fact]
    public void Unknown_language_in_settings_falls_back_to_auto()
    {
        var s = new Settings();
        s.Audio.Language = "xx";
        Assert.Equal("auto", WorkerSettings.From(s, @"Z:\no-such-dir").AudioLanguage);
        s.Audio.Language = "DE";
        Assert.Equal("de", WorkerSettings.From(s, @"Z:\no-such-dir").AudioLanguage);
    }

    [Fact]
    public async Task Unknown_language_is_a_bad_request_not_a_device_failure()
    {
        var home = TestEnv.NewDir("lang");
        var s = TestEnv.Settings(home, x => x.Inference.Fake = true);
        await using var h = await PipeHarness.StartAsync(new FakeEngine(), s);
        var (r, _) = await h.CallAsync(new WorkerRequest(0, "transcribe", Format: "f32", SampleRate: 16000, Channels: 1, Source: "mic", Language: "xx"), TestEnv.Pcm(0.2f));
        Assert.Equal(WorkerErrors.BadRequest, r.Error);
    }

    // ---------------- D27: lenient settings.json ----------------

    [Fact]
    public void Settings_with_bom_comments_and_trailing_commas_are_read()
    {
        var home = TestEnv.NewDir("lenient");
        var json = """
            {
              // set by hand
              "inference": { "device": "cpu", "idleExitS": 77, },
              "log": { "level": "debug" },
            }
            """;
        File.WriteAllText(DataPaths.SettingsFile(home), json, new UTF8Encoding(encoderShouldEmitUTF8Identifier: true));
        var s = WorkerSettings.Load(home);
        Assert.Equal("cpu", s.Device);
        Assert.Equal(77, s.IdleExitS);
        Assert.Equal("debug", s.LogLevel);
    }

    [Fact]
    public void A_key_of_the_wrong_type_keeps_only_that_default()
    {
        var parsed = WorkerSettings.Parse("""{ "inference": { "device": "cpu", "idleExitS": "soon" }, "index": { "dim": 512 } }""");
        Assert.Equal("cpu", parsed.Inference.Device);
        Assert.Equal(new Settings().Inference.IdleExitS, parsed.Inference.IdleExitS);
        Assert.Equal(512, parsed.Index.Dim);
    }

    // ---------------- C13: device lost after load ----------------

    [Fact]
    public void Device_lost_after_load_is_planned_again_on_the_next_device()
    {
        var home = TestEnv.NewDir("devlost");
        TestEnv.InstallEmbeddingGemma(Path.Combine(home, "models"));
        var s = TestEnv.Settings(home, x => x.Inference.Device = "gpu");
        var gpu = new FlakyGpuBackend();
        using var engine = new RealEngine(s, home, gpu, new OnnxBackend(cpuOnly: true));
        Assert.Equal("GPU", engine.Hello(768).Device);
        Assert.NotEmpty(engine.EmbedText("before", TextKind.Query));

        gpu.Broken = true;   // TDR / driver update: every run on the loaded GPU session fails from now on
        for (int i = 0; i < RealEngine.RunFailuresBeforeReplan; i++)
            Assert.Throws<DeviceFailedException>(() => engine.EmbedText("lost", TextKind.Query));
        Assert.Contains("GPU.0", engine.FailedDevices);
        // The next request loads the embedder on the next candidate (the CPU) instead of failing for ever.
        Assert.NotEmpty(engine.EmbedText("after", TextKind.Query));
        Assert.Equal("CPU", engine.Hello(768).Device);
        // device.json keeps the GPU: the next worker process tries it again.
        var file = JsonSerializer.Deserialize(File.ReadAllBytes(DataPaths.DeviceCache(home)), Models.WorkerJson.Default.DeviceCacheFile)!;
        Assert.Equal("GPU.0", file.Chosen!.DeviceId);
    }

    [Fact]
    public void One_failed_run_does_not_move_the_device()
    {
        var home = TestEnv.NewDir("devonce");
        TestEnv.InstallEmbeddingGemma(Path.Combine(home, "models"));
        var s = TestEnv.Settings(home, x => x.Inference.Device = "gpu");
        var gpu = new FlakyGpuBackend();
        using var engine = new RealEngine(s, home, gpu, new OnnxBackend(cpuOnly: true));
        Assert.Equal("GPU", engine.Hello(768).Device);
        gpu.Broken = true;
        Assert.Throws<DeviceFailedException>(() => engine.EmbedText("glitch", TextKind.Query));
        gpu.Broken = false;
        Assert.NotEmpty(engine.EmbedText("fine", TextKind.Query));
        Assert.Empty(engine.FailedDevices);
        Assert.Equal("GPU", engine.Hello(768).Device);
    }

    /// <summary>Main backend with one GPU whose graphs run on the ONNX Runtime CPU; Broken makes every GPU run (and load) fail.</summary>
    private sealed class FlakyGpuBackend : IInferenceBackend
    {
        private readonly OnnxBackend _cpu = new(cpuOnly: true);
        public volatile bool Broken;
        public string Name => "Fake";
        public string Version => "1";
        public string NpuDriverVersion => "";
        public IReadOnlyList<DeviceInfo> Devices { get; } =
        [
            new(DeviceKind.Gpu, "GPU.0", "Fake GPU", "OpenVINO", Integrated: true),
            new(DeviceKind.Cpu, "CPU", "CPU", "OpenVINO"),
        ];

        public IInferenceSession Load(string modelPath, DeviceInfo device, string? cacheDir, IReadOnlyDictionary<string, long[]>? staticShapes = null)
        {
            if (Broken && device.Kind == DeviceKind.Gpu) throw new DeviceFailedException("GPU device removed");
            var inner = _cpu.Load(modelPath, _cpu.Devices[^1], null);
            return device.Kind == DeviceKind.Gpu ? new FlakySession(inner, device, this) : inner;
        }

        public void Dispose() => _cpu.Dispose();
    }

    private sealed class FlakySession(IInferenceSession inner, DeviceInfo device, FlakyGpuBackend owner) : IInferenceSession
    {
        public DeviceInfo Device => device;
        public IReadOnlyList<TensorSpec> Inputs => inner.Inputs;
        public IReadOnlyList<TensorSpec> Outputs => inner.Outputs;
        public IReadOnlyDictionary<string, HostTensor> Run(IReadOnlyList<HostTensor> inputs)
            => owner.Broken ? throw new DeviceFailedException("GPU device removed") : inner.Run(inputs);
        public void Dispose() => inner.Dispose();
    }
}
