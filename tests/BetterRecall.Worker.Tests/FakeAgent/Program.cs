// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Buffers.Binary;
using System.Diagnostics;
using System.IO.Pipes;
using System.Text;
using System.Text.Json;
using BetterRecall.Contracts;

// FakeAgent <workerExe> <home> <out.json> [--real] [--wav <path>] [--settings <json>] [--space]
// Creates the private pipe, starts the worker, sends hello, embed_text (query + document), embed_image (bgra8),
// transcribe, shutdown; writes every response header plus the worker exit code to out.json.
// --settings is handed to the worker on stdin (--settings-stdin) as the agent's worker keys (sweep 4 C5). --space sends hello, bench, embed_text
// and shutdown only, to see which embedder each reply names (sweep 4 A-M3). --measure (Rust port W4) times start-up,
// hello, five text and three 1280x720 image embeds and one OCR read, then reads the worker's memory and its CPU time over
// a 30 s idle window; no bench, so it stays light on a PC in use.
var workerExe = args[0];
var home = args[1];
var outFile = args[2];
bool real = false, space = false, measure = false;
string? wavPath = null, workerSettings = null;
for (int i = 3; i < args.Length; i++)
{
    if (args[i] == "--real") real = true;
    else if (args[i] == "--space") space = true;
    else if (args[i] == "--measure") measure = true;
    else if (args[i] == "--wav" && i + 1 < args.Length) wavPath = args[++i];
    else if (args[i] == "--settings" && i + 1 < args.Length)
    {
        workerSettings = args[++i];
        if (workerSettings.StartsWith('@')) workerSettings = File.ReadAllText(workerSettings[1..]);   // @file: JSON that a shell would mangle
    }
}

var name = PipeNames.NewWorker();
await using var server = new NamedPipeServerStream(name, PipeDirection.InOut, 1, PipeTransmissionMode.Byte, PipeOptions.Asynchronous);
var started = Stopwatch.StartNew();
var psi = new ProcessStartInfo(workerExe, ["--pipe", name, "--home", home]) { UseShellExecute = false };
if (workerSettings is not null) { psi.ArgumentList.Add("--settings-stdin"); psi.RedirectStandardInput = true; }
using var worker = Process.Start(psi)!;
if (workerSettings is not null)
{
    worker.StandardInput.BaseStream.Write(new UTF8Encoding(false).GetBytes(workerSettings));
    worker.StandardInput.Close();
}
var lines = new List<string>();
try
{
    using var cts = new CancellationTokenSource(real || measure ? TimeSpan.FromMinutes(30) : TimeSpan.FromSeconds(60));
    await server.WaitForConnectionAsync(cts.Token);
    long id = 0;
    async Task Send(WorkerRequest r, byte[] payload)
    {
        var header = JsonSerializer.SerializeToUtf8Bytes(r, ContractsJson.Default.WorkerRequest);
        var sw = Stopwatch.StartNew();
        await BinaryFrame.WriteAsync(server, header, payload, cts.Token);
        var f = await BinaryFrame.ReadAsync(server, cts.Token) ?? throw new EndOfStreamException();
        sw.Stop();
        var resp = JsonSerializer.Deserialize(f.Header, ContractsJson.Default.WorkerResponse)!;
        var suffix = real || measure ? $" payload={f.Payload.Length} ms={sw.ElapsedMilliseconds}" : $" payload={f.Payload.Length}";
        lines.Add(JsonSerializer.Serialize(resp, ContractsJson.Default.WorkerResponse) + suffix);
    }
    if (measure)
    {
        lines.Add($"measure connect_ms={started.ElapsedMilliseconds}");
        await Send(new WorkerRequest(++id, "hello"), []);
        lines.Add($"measure hello_done_ms={started.ElapsedMilliseconds}");
        string[] texts = ["quarterly sales figures", "meeting notes from monday", "invoice 2026 total", "holiday photos beach", "rust port plan"];
        foreach (var text in texts) await Send(new WorkerRequest(++id, "embed_text", Text: text, Kind: "query"), []);
        const int mw = 1280, mh = 720;
        var img = new byte[mw * mh * 4];
        for (int i = 0; i < img.Length; i++) img[i] = (byte)((i * 31) ^ (i >> 9));
        for (int k = 0; k < 3; k++)
            await Send(new WorkerRequest(++id, "embed_image", Format: "bgra8", Width: mw, Height: mh, Stride: mw * 4, Nsfw: 0.8), img);
        await Send(new WorkerRequest(++id, "ocr", Format: "bgra8", Width: mw, Height: mh, Stride: mw * 4), img);
        worker.Refresh();
        lines.Add($"measure ws_mb={worker.WorkingSet64 / 1048576.0:F1} peak_ws_mb={worker.PeakWorkingSet64 / 1048576.0:F1} private_mb={worker.PrivateMemorySize64 / 1048576.0:F1} cpu_s={worker.TotalProcessorTime.TotalSeconds:F2}");
        var cpu0 = worker.TotalProcessorTime;
        var idle = Stopwatch.StartNew();
        await Task.Delay(TimeSpan.FromSeconds(30), cts.Token);
        worker.Refresh();
        var idlePct = (worker.TotalProcessorTime - cpu0).TotalMilliseconds / idle.Elapsed.TotalMilliseconds / Environment.ProcessorCount * 100;
        lines.Add($"measure idle_cpu_pct={idlePct:F3} idle_cpu_ms={(worker.TotalProcessorTime - cpu0).TotalMilliseconds:F0} ws_after_idle_mb={worker.WorkingSet64 / 1048576.0:F1}");
        await Send(new WorkerRequest(++id, "shutdown"), []);
    }
    else if (space)
    {
        await Send(new WorkerRequest(++id, "hello"), []);
        await Send(new WorkerRequest(++id, "bench"), []);
        await Send(new WorkerRequest(++id, "embed_text", Text: "hello world", Kind: "document"), []);
        await Send(new WorkerRequest(++id, "shutdown"), []);
    }
    else if (!real)
    {
        await Send(new WorkerRequest(++id, "hello"), []);
        await Send(new WorkerRequest(++id, "embed_text", Text: "hello world", Kind: "query"), []);
        await Send(new WorkerRequest(++id, "embed_text", Text: "hello world", Kind: "document"), []);
        var bgra = new byte[64 * 32 * 4];
        for (int i = 0; i < bgra.Length; i++) bgra[i] = (byte)(i * 7);
        await Send(new WorkerRequest(++id, "embed_image", Format: "bgra8", Width: 64, Height: 32, Stride: 256, Nsfw: 0.8), bgra);
        var pcm = new float[16000];
        for (int i = 0; i < pcm.Length; i++) pcm[i] = 0.1f * MathF.Sin(i * 0.05f);
        await Send(new WorkerRequest(++id, "transcribe", Format: "f32", SampleRate: 16000, Channels: 1, Source: "mic", Language: "auto"), BinaryFrame.FloatsToBytes(pcm));
        await Send(new WorkerRequest(++id, "shutdown"), []);
    }
    else
    {
        await Send(new WorkerRequest(++id, "hello"), []);
        await Send(new WorkerRequest(++id, "bench"), []);
        await Send(new WorkerRequest(++id, "embed_text", Text: "quarterly sales figures", Kind: "query"), []);
        await Send(new WorkerRequest(++id, "embed_text", Text: "Tabelle mit Umsatzzahlen für das dritte Quartal", Kind: "document"), []);
        const int w = 1280, h = 720;
        var bgra = new byte[w * h * 4];
        for (int y = 0; y < h; y++)
            for (int x = 0; x < w; x++)
            {
                int o = (y * w + x) * 4;
                bool grid = x % 64 == 0 || y % 48 == 0;
                bool block = (x / 160 + y / 120) % 3 == 0;
                byte r = (byte)(grid ? 30 : block ? 220 : x * 255 / w);
                byte g = (byte)(grid ? 30 : block ? 230 : y * 255 / h);
                byte b = (byte)(grid ? 30 : block ? 240 : (x + y) % 256);
                bgra[o] = b;
                bgra[o + 1] = g;
                bgra[o + 2] = r;
                bgra[o + 3] = 255;
            }
        await Send(new WorkerRequest(++id, "embed_image", Format: "bgra8", Width: w, Height: h, Stride: w * 4, Nsfw: 0.8), bgra);
        if (!string.IsNullOrWhiteSpace(wavPath))
        {
            var samples = ReadWav16(wavPath);
            var wavPayload = BinaryFrame.FloatsToBytes(samples);
            await Send(new WorkerRequest(++id, "transcribe", Format: "f32", SampleRate: 16000, Channels: 1, Source: "mic", Language: "auto"), wavPayload);
            await Send(new WorkerRequest(++id, "transcribe", Format: "f32", SampleRate: 16000, Channels: 1, Source: "mic", Language: "auto"), wavPayload);
        }
        var zeroSamples = new float[16000];
        await Send(new WorkerRequest(++id, "transcribe", Format: "f32", SampleRate: 16000, Channels: 1, Source: "mic", Language: "auto"), BinaryFrame.FloatsToBytes(zeroSamples));
        await Send(new WorkerRequest(++id, "shutdown"), []);
    }
}
catch (Exception e) { lines.Add("agent error: " + e.GetType().Name + ": " + e.Message); }
if (!worker.WaitForExit(real || measure ? 60000 : 15000)) { worker.Kill(); lines.Add("worker killed"); }
lines.Add("exit=" + worker.ExitCode);
File.WriteAllLines(outFile, lines);

static float[] ReadWav16(string path)
{
    using var fs = File.OpenRead(path);
    using var reader = new BinaryReader(fs);
    if (fs.Length < 12) throw new InvalidDataException("File too short for WAV header.");
    var riff = reader.ReadBytes(4);
    if (Encoding.ASCII.GetString(riff) != "RIFF") throw new InvalidDataException("Not a RIFF file.");
    reader.ReadUInt32();
    var wave = reader.ReadBytes(4);
    if (Encoding.ASCII.GetString(wave) != "WAVE") throw new InvalidDataException("Not a WAVE file.");

    ushort format = 0;
    ushort channels = 0;
    uint sampleRate = 0;
    ushort bitsPerSample = 0;
    bool hasFmt = false;
    byte[]? dataBytes = null;

    while (fs.Position < fs.Length)
    {
        if (fs.Position + 8 > fs.Length) break;
        var chunkIdBytes = reader.ReadBytes(4);
        string chunkId = Encoding.ASCII.GetString(chunkIdBytes);
        uint chunkSize = reader.ReadUInt32();

        if (chunkId == "fmt ")
        {
            if (chunkSize < 16) throw new InvalidDataException($"'fmt ' chunk too small: {chunkSize}");
            format = reader.ReadUInt16();
            channels = reader.ReadUInt16();
            sampleRate = reader.ReadUInt32();
            reader.ReadUInt32();
            reader.ReadUInt16();
            bitsPerSample = reader.ReadUInt16();
            hasFmt = true;
            if (chunkSize > 16)
            {
                reader.BaseStream.Seek((long)(chunkSize - 16), SeekOrigin.Current);
            }
        }
        else if (chunkId == "data")
        {
            dataBytes = reader.ReadBytes((int)chunkSize);
        }
        else
        {
            reader.BaseStream.Seek((long)chunkSize, SeekOrigin.Current);
        }

        if ((chunkSize & 1) != 0 && fs.Position < fs.Length)
        {
            reader.BaseStream.Seek(1, SeekOrigin.Current);
        }
    }

    if (!hasFmt) throw new InvalidDataException("Missing 'fmt ' chunk in WAV.");
    if (dataBytes is null) throw new InvalidDataException("Missing 'data' chunk in WAV.");
    if (format != 1 || bitsPerSample != 16 || channels != 1 || sampleRate != 16000)
    {
        throw new InvalidDataException($"Expected PCM format 1, 16 bits, 1 channel, 16000 Hz, but got format={format}, bits={bitsPerSample}, channels={channels}, rate={sampleRate}.");
    }

    int sampleCount = dataBytes.Length / 2;
    var samples = new float[sampleCount];
    for (int i = 0; i < sampleCount; i++)
    {
        short sample = BinaryPrimitives.ReadInt16LittleEndian(dataBytes.AsSpan(i * 2, 2));
        samples[i] = sample / 32768f;
    }
    return samples;
}
