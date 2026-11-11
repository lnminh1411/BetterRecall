// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Diagnostics;
using BetterRecall.Contracts;
using BetterRecall.Worker.Engine;
using BetterRecall.Worker.Hosting;
using BetterRecall.Worker.Vectors;
using Xunit.Abstractions;

namespace BetterRecall.Worker.Tests;

public class RealModelTests(ITestOutputHelper output)
{
    private static string RepoRoot()
    {
        var d = new DirectoryInfo(AppContext.BaseDirectory);
        while (d is not null && !File.Exists(Path.Combine(d.FullName, "BetterRecall.slnx"))) d = d.Parent;
        return d?.FullName ?? throw new InvalidOperationException("repo root not found");
    }

    private static string BuildDir(string project)
    {
        var pivot = new DirectoryInfo(AppContext.BaseDirectory.TrimEnd('\\')).Name;
        var dir = Path.Combine(RepoRoot(), "artifacts", "bin", project, pivot);
        if (!Directory.Exists(dir)) dir = Directory.GetDirectories(Path.Combine(RepoRoot(), "artifacts", "bin", project)).First();
        return dir;
    }

    private static void CopyTree(string from, string to)
    {
        Directory.CreateDirectory(to);
        foreach (var f in Directory.GetFiles(from, "*", SearchOption.AllDirectories))
        {
            var dst = Path.Combine(to, Path.GetRelativePath(from, f));
            Directory.CreateDirectory(Path.GetDirectoryName(dst)!);
            File.Copy(f, dst, true);
        }
    }

    [Fact]
    public async Task Real_models_end_to_end_in_process()
    {
        var models = Environment.GetEnvironmentVariable("BR_REAL_MODELS");
        if (string.IsNullOrWhiteSpace(models) || !Directory.Exists(models))
        {
            output.WriteLine("skipped: BR_REAL_MODELS not set");
            return;
        }

        var home = Environment.GetEnvironmentVariable("BR_REAL_HOME");
        Assert.True(!string.IsNullOrWhiteSpace(home), "BR_REAL_HOME environment variable is required when BR_REAL_MODELS is set.");

        var device = Environment.GetEnvironmentVariable("BR_REAL_DEVICE");
        if (string.IsNullOrWhiteSpace(device)) device = "auto";

        if (File.Exists(DataPaths.DeviceCache(home)))
        {
            File.Delete(DataPaths.DeviceCache(home));
        }

        var settings = TestEnv.Settings(home, s =>
        {
            s.Inference.Fake = false;
            s.Inference.ModelDir = models;
            s.Inference.ImageTokens = 140;
            s.Inference.Device = device;
            s.Index.Dim = 768;
        });

        using var engine = new RealEngine(settings, home);
        await using var h = await PipeHarness.StartAsync(engine, settings, TimeSpan.FromMinutes(30));
        var timeout = TimeSpan.FromMinutes(20);
        var sw = new Stopwatch();

        // hello
        sw.Restart();
        var (rHello, _) = await h.CallAsync(new WorkerRequest(0, "hello"), timeout: timeout);
        sw.Stop();
        Assert.True(rHello.Ok, rHello.Error);
        Assert.NotNull(rHello.Hello);
        output.WriteLine($"hello: Device={rHello.Hello.Device}, Ep={rHello.Hello.Ep}, Model={rHello.Hello.Model}, Dim={rHello.Hello.Dim}, Asr={rHello.Hello.Asr}, Vad={rHello.Hello.Vad}, Missing=[{string.Join(", ", rHello.Hello.Missing)}], took={sw.ElapsedMilliseconds}ms");
        Assert.Empty(rHello.Hello.Missing);

        // bench
        sw.Restart();
        var (rBench, _) = await h.CallAsync(new WorkerRequest(0, "bench"), timeout: timeout);
        sw.Stop();
        Assert.True(rBench.Ok, rBench.Error);
        Assert.NotNull(rBench.Bench);
        output.WriteLine($"bench: Chosen={rBench.Bench.Chosen}, took={sw.ElapsedMilliseconds}ms");
        foreach (var d in rBench.Bench.Devices)
        {
            output.WriteLine($"Device={d.Device}, Ep={d.Ep}, Model={d.Model}, TextDevice={d.TextDevice}, Ok={d.Ok}, ImageMs={d.ImageMs}, TextMs={d.TextMs}, Cosine={d.Cosine}, CompileMs={d.CompileMs}, Error={d.Error}");
        }
        if (rBench.Bench.Tokens is not null)
        {
            foreach (var tb in rBench.Bench.Tokens)
            {
                output.WriteLine($"TokenBench: Device={tb.Device}, ImageTokens={tb.ImageTokens}, ImageMs={tb.ImageMs}");
            }
        }
        Assert.True(File.Exists(DataPaths.DeviceCache(home)));
        output.WriteLine($"device.json:\n{File.ReadAllText(DataPaths.DeviceCache(home))}");
        Assert.NotNull(rBench.Bench.Tokens);
        var chosen = rBench.Bench.Chosen;
        Assert.Contains(rBench.Bench.Tokens, t => t.Device == chosen && t.ImageTokens == 70);
        Assert.Contains(rBench.Bench.Tokens, t => t.Device == chosen && t.ImageTokens == 140);
        Assert.Contains(rBench.Bench.Tokens, t => t.Device == chosen && t.ImageTokens == 280);

        // embed_text
        sw.Restart();
        var (rq, pq) = await h.CallAsync(new WorkerRequest(0, "embed_text", Text: "quarterly sales figures", Kind: "query"), timeout: timeout);
        sw.Stop();
        Assert.True(rq.Ok, rq.Error);
        var qVec = BinaryFrame.BytesToFloats(pq);
        Assert.Equal(768, qVec.Length);
        long qMs = sw.ElapsedMilliseconds;

        sw.Restart();
        var (rs, ps) = await h.CallAsync(new WorkerRequest(0, "embed_text", Text: "Tabelle mit Umsatzzahlen für das dritte Quartal", Kind: "document"), timeout: timeout);
        sw.Stop();
        Assert.True(rs.Ok, rs.Error);
        var sVec = BinaryFrame.BytesToFloats(ps);
        Assert.Equal(768, sVec.Length);
        long sMs = sw.ElapsedMilliseconds;

        sw.Restart();
        var (rc, pc) = await h.CallAsync(new WorkerRequest(0, "embed_text", Text: "a cat sleeping on a sofa", Kind: "document"), timeout: timeout);
        sw.Stop();
        Assert.True(rc.Ok, rc.Error);
        var cVec = BinaryFrame.BytesToFloats(pc);
        Assert.Equal(768, cVec.Length);
        long cMs = sw.ElapsedMilliseconds;

        double cosSales = VectorMath.Cosine(qVec, sVec);
        double cosCat = VectorMath.Cosine(qVec, cVec);
        output.WriteLine($"embed_text: query ms={qMs}, sales doc ms={sMs}, cat doc ms={cMs}, cos(query, sales)={cosSales:F4}, cos(query, cat)={cosCat:F4}");
        Assert.True(cosSales > cosCat, $"Expected cosine(query, sales) > cosine(query, cat), but got {cosSales} <= {cosCat}");

        // embed_image
        var benchImg = RealEngine.BenchImage();
        var bgra = TestEnv.ToBgra(benchImg);
        var imgReq = new WorkerRequest(0, "embed_image", Format: "bgra8", Width: benchImg.Width, Height: benchImg.Height, Stride: benchImg.Width * 4, Nsfw: 0.8);

        sw.Restart();
        var (rImg1, pImg1) = await h.CallAsync(imgReq, bgra, timeout: timeout);
        sw.Stop();
        Assert.True(rImg1.Ok, rImg1.Error);
        Assert.Null(rImg1.Dropped);
        Assert.Equal(768 * 4, pImg1.Length);
        long img1Ms = sw.ElapsedMilliseconds;

        sw.Restart();
        var (rImg2, pImg2) = await h.CallAsync(imgReq, bgra, timeout: timeout);
        sw.Stop();
        Assert.True(rImg2.Ok, rImg2.Error);
        Assert.Null(rImg2.Dropped);
        Assert.Equal(768 * 4, pImg2.Length);
        long img2Ms = sw.ElapsedMilliseconds;

        output.WriteLine($"embed_image: #1 ms={img1Ms} (worker {rImg1.Ms} ms), #2 ms={img2Ms} (worker {rImg2.Ms} ms)");

        // transcribe (optional)
        var wav = Environment.GetEnvironmentVariable("BR_REAL_WAV");
        if (!string.IsNullOrWhiteSpace(wav) && File.Exists(wav))
        {
            var samples = TestEnv.ReadWav16(wav);
            var wavPayload = BinaryFrame.FloatsToBytes(samples);
            double audioSec = samples.Length / 16000.0;
            double audioMs = audioSec * 1000.0;
            var wavReq = new WorkerRequest(0, "transcribe", Format: "f32", SampleRate: 16000, Channels: 1, Source: "mic", Language: "auto");

            for (int run = 1; run <= 2; run++)
            {
                sw.Restart();
                var (rWav, _) = await h.CallAsync(wavReq, wavPayload, timeout: timeout);
                sw.Stop();
                Assert.True(rWav.Ok, rWav.Error);
                Assert.NotNull(rWav.Transcript);
                var t = rWav.Transcript;
                double rtf = audioMs > 0 ? t.Ms / audioMs : 0;
                output.WriteLine($"transcribe #{run}: wall ms={sw.ElapsedMilliseconds}, Transcript.Ms={t.Ms}, audio sec={audioSec:F2}, RTF={rtf:F3}, Speech={t.Speech}, Language={t.Language}, Text=\"{t.Text}\", Segments={t.Segments.Length}");
                Assert.True(t.Speech);
                Assert.False(string.IsNullOrWhiteSpace(t.Text));
            }
        }

        // transcribe 1 s zeros
        var zeroSamples = new float[16000];
        var zeroPayload = BinaryFrame.FloatsToBytes(zeroSamples);
        var zeroReq = new WorkerRequest(0, "transcribe", Format: "f32", SampleRate: 16000, Channels: 1, Source: "mic", Language: "auto");
        sw.Restart();
        var (rZero, _) = await h.CallAsync(zeroReq, zeroPayload, timeout: timeout);
        sw.Stop();
        Assert.True(rZero.Ok, rZero.Error);
        Assert.NotNull(rZero.Transcript);
        Assert.False(rZero.Transcript.Speech);
        output.WriteLine($"transcribe zeros: wall ms={sw.ElapsedMilliseconds}, Transcript.Ms={rZero.Transcript.Ms}, Speech={rZero.Transcript.Speech}");

        // worker log
        var logsDir = DataPaths.LogsDir(home);
        if (Directory.Exists(logsDir))
        {
            var logFiles = Directory.GetFiles(logsDir, "worker-*.log");
            if (logFiles.Length > 0)
            {
                var lastLog = logFiles.OrderByDescending(File.GetLastWriteTimeUtc).First();
                output.WriteLine($"--- worker log: {Path.GetFileName(lastLog)} ---");
                foreach (var line in File.ReadLines(lastLog))
                {
                    if (line.Contains("bench", StringComparison.OrdinalIgnoreCase) ||
                        line.Contains("device", StringComparison.OrdinalIgnoreCase) ||
                        line.Contains("whisper", StringComparison.OrdinalIgnoreCase) ||
                        line.Contains("dropped", StringComparison.OrdinalIgnoreCase))
                    {
                        output.WriteLine(line);
                    }
                }
            }
        }
    }

    [Fact]
    public void Real_published_worker_exe_under_fake_agent()
    {
        var models = Environment.GetEnvironmentVariable("BR_REAL_MODELS");
        if (string.IsNullOrWhiteSpace(models) || !Directory.Exists(models))
        {
            output.WriteLine("skipped: BR_REAL_MODELS not set");
            return;
        }

        var realHome = Environment.GetEnvironmentVariable("BR_REAL_HOME");
        Assert.True(!string.IsNullOrWhiteSpace(realHome), "BR_REAL_HOME environment variable is required when BR_REAL_MODELS is set.");

        var workerDir = Environment.GetEnvironmentVariable("BR_REAL_WORKER_DIR");
        if (string.IsNullOrWhiteSpace(workerDir) || !Directory.Exists(workerDir))
        {
            output.WriteLine("skipped: BR_REAL_WORKER_DIR not set");
            return;
        }

        var parent = Directory.GetParent(Path.GetFullPath(realHome))?.FullName
                     ?? throw new InvalidOperationException("BR_REAL_HOME has no parent directory");
        var root = Path.Combine(parent, "e2e-exe");
        if (Directory.Exists(root))
        {
            Directory.Delete(root, recursive: true);
        }
        Directory.CreateDirectory(root);

        var install = Path.Combine(root, "install");
        CopyTree(workerDir, Path.Combine(install, "worker"));
        CopyTree(BuildDir("FakeAgent"), install);
        File.Move(Path.Combine(install, "FakeAgent.exe"), Path.Combine(install, InstallLayout.AgentExe));

        var home = Path.Combine(root, "home");
        var device = Environment.GetEnvironmentVariable("BR_REAL_DEVICE");
        if (string.IsNullOrWhiteSpace(device)) device = "auto";
        TestEnv.Settings(home, s =>
        {
            s.Inference.Fake = false;
            s.Inference.ModelDir = models;
            s.Inference.ImageTokens = 140;
            s.Inference.Device = device;
            s.Index.Dim = 768;
            s.Security.MemoryIsolation = true;
        });
        if (File.Exists(DataPaths.DeviceCache(home)))
        {
            File.Delete(DataPaths.DeviceCache(home));
        }

        var workerExe = InstallLayout.Full(install, InstallLayout.WorkerExe);
        var outFile = Path.Combine(root, "out.txt");
        var agentArgs = new List<string> { workerExe, home, outFile, "--real" };
        var wav = Environment.GetEnvironmentVariable("BR_REAL_WAV");
        if (!string.IsNullOrWhiteSpace(wav))
        {
            agentArgs.Add("--wav");
            agentArgs.Add(wav);
        }

        var psi = new ProcessStartInfo(Path.Combine(install, InstallLayout.AgentExe), agentArgs) { UseShellExecute = false };
        using var agent = Process.Start(psi)!;
        Assert.True(agent.WaitForExit((int)TimeSpan.FromMinutes(30).TotalMilliseconds), "fake agent timed out");

        var lines = File.ReadAllLines(outFile);
        foreach (var l in lines) output.WriteLine(l);
        Assert.NotEmpty(lines);
        Assert.Equal("exit=0", lines[^1]);
        Assert.DoesNotContain(lines, l => l.StartsWith("agent error", StringComparison.OrdinalIgnoreCase));
        var helloLine = lines.FirstOrDefault(l => l.Contains("\"missing\":", StringComparison.OrdinalIgnoreCase)) ?? lines[0];
        Assert.Contains("\"missing\":[]", helloLine);
    }
}
