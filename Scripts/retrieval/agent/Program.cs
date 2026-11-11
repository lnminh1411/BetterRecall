// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Diagnostics;
using System.IO.Pipes;
using System.Text.Json;
using BetterRecall.Contracts;

// BetterRecall.Agent(retrieval harness) <home> <imgDir> <out.json>
var home = args[0]; var imgDir = args[1]; var outFile = args[2];
var set = JsonDocument.Parse(File.ReadAllText(Path.Combine(imgDir, "set.json"))).RootElement;
var workerExe = Path.Combine(AppContext.BaseDirectory, "worker", "BetterRecall.Worker.exe");
var name = PipeNames.NewWorker();
await using var server = new NamedPipeServerStream(name, PipeDirection.InOut, 1, PipeTransmissionMode.Byte, PipeOptions.Asynchronous);
using var worker = Process.Start(new ProcessStartInfo(workerExe, ["--pipe", name, "--home", home]) { UseShellExecute = false })!;
using var cts = new CancellationTokenSource(TimeSpan.FromMinutes(30));
await server.WaitForConnectionAsync(cts.Token);
long id = 0;
async Task<(WorkerResponse R, float[] V, long Ms)> Send(WorkerRequest r, byte[] payload)
{
    var sw = Stopwatch.StartNew();
    await BinaryFrame.WriteAsync(server, JsonSerializer.SerializeToUtf8Bytes(r, ContractsJson.Default.WorkerRequest), payload, cts.Token);
    var f = await BinaryFrame.ReadAsync(server, cts.Token) ?? throw new EndOfStreamException();
    var resp = JsonSerializer.Deserialize(f.Header, ContractsJson.Default.WorkerResponse)!;
    if (!resp.Ok) throw new Exception($"{r.Op}: {resp.Error}");
    return (resp, BinaryFrame.BytesToFloats(f.Payload), sw.ElapsedMilliseconds);
}
var hello = await Send(new WorkerRequest(++id, "hello"), []);
var res = new Dictionary<string, object>
{
    ["device"] = hello.R.Hello!.Device, ["model"] = hello.R.Hello.Model, ["helloMs"] = hello.Ms,
};
var images = new Dictionary<string, float[]>(); var imgMs = new List<long>();
foreach (var img in set.GetProperty("images").EnumerateArray())
{
    var bytes = File.ReadAllBytes(Path.Combine(imgDir, img.GetString() + ".png"));
    var r = await Send(new WorkerRequest(++id, "embed_image", Format: "png", Width: 1280, Height: 720), bytes);
    images[img.GetString()!] = r.V; imgMs.Add(r.Ms);
}
var texts = new List<string>();
foreach (var p in set.GetProperty("queries").EnumerateObject()) foreach (var q in p.Value.EnumerateArray()) texts.Add(q.GetString()!);
foreach (var q in set.GetProperty("negatives").EnumerateArray()) texts.Add(q.GetString()!);
var queries = new Dictionary<string, float[]>(); var qMs = new List<long>();
foreach (var t in texts) { var r = await Send(new WorkerRequest(++id, "embed_text", Text: t, Kind: "query"), []); queries[t] = r.V; qMs.Add(r.Ms); }
await Send(new WorkerRequest(++id, "shutdown"), []).ContinueWith(_ => { });
res["imageMs"] = imgMs; res["queryMs"] = qMs; res["images"] = images; res["queries"] = queries;
File.WriteAllText(outFile, JsonSerializer.Serialize(res, new JsonSerializerOptions { TypeInfoResolver = new System.Text.Json.Serialization.Metadata.DefaultJsonTypeInfoResolver() }));
worker.WaitForExit(30000);
Console.WriteLine($"ok device={res["device"]} model={res["model"]} hello={hello.Ms}ms image≈{imgMs.Skip(1).DefaultIfEmpty(0).Average():0}ms query≈{qMs.Skip(1).DefaultIfEmpty(0).Average():0}ms");
