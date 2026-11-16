// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Diagnostics;
using System.IO.Pipes;
using System.Runtime.InteropServices;
using System.Text.Json;
using BetterRecall.Agent.Privacy;
using BetterRecall.Agent.Worker;
using BetterRecall.Contracts;

namespace BetterRecall.Agent.Tests;

/// <summary>Bug sweep 3, batch I: A3 (card next to more digits) and A2 (OCR timeout during a blocked write).</summary>
public class Sweep3IRedactionTests
{
    private static readonly RedactionOptions Defaults = RedactionOptions.From(new PrivacySettings());

    [Theory]
    [InlineData("4111 1111 1111 1111 12/26", "12/26")]
    [InlineData("4111 1111 1111 1111 123", "123")]
    [InlineData("4111-1111-1111-1111 07", "07")]
    [InlineData("4012-8888-8888-1881 07", "07")]
    [InlineData("Order 12 4111 1111 1111 1111", "Order 12")]
    [InlineData("Card 5500 0000 0000 0004 123", "Card 123")]
    [InlineData("Order 4111 1111 1111 1112 12/26", "Order 4111 1111 1111 1112 12/26")]   // no window passes Luhn
    [InlineData("Serial 41111111111111112", "Serial 41111111111111112")]               // one group: never cut inside
    public void A3_card_followed_or_preceded_by_digits_is_redacted(string line, string kept)
    {
        var r = Redactor.Apply([Ocr.Line(line, 0, 0)], [], Defaults);
        Assert.Equal(kept, r.Text);
        if (kept != line) Assert.NotEmpty(r.Boxes);   // the keyframe pixels are blacked out too
    }

    [Fact]
    public void A3_window_is_the_card_groups_only()
    {
        Assert.Equal((0, 19), Redactor.CardSpan("4111 1111 1111 1111 12"));
        Assert.Equal((3, 19), Redactor.CardSpan("12 4111 1111 1111 1111"));
        Assert.Equal((0, 19), Redactor.CardSpan("4111 1111 1111 1111"));
        Assert.Null(Redactor.CardSpan("4111 1111 1111 1112 12"));
    }
}

/// <summary>Worker served in-process; transcribe takes TranscribeMs (a cold Whisper call). Pids are made up and counted.</summary>
internal sealed class SlowWorkerLauncher : IWorkerLauncher
{
    private int _pid = 41000;
    public int TranscribeMs = 3000;
    /// <summary>Answer the first ocr with a frame whose header is not JSON (a broken worker).</summary>
    public bool GarbageOnOcr;
    public readonly List<int> Started = [];
    public readonly List<int> Killed = [];

    public int Start(string pipe, string home)
    {
        int pid;
        lock (Started) { pid = ++_pid; Started.Add(pid); }
        _ = Task.Run(async () =>
        {
            try
            {
                using var c = new NamedPipeClientStream(".", pipe, PipeDirection.InOut, PipeOptions.Asynchronous);
                await c.ConnectAsync(5000);
                while (true)
                {
                    var f = await BinaryFrame.ReadAsync(c, CancellationToken.None);
                    if (f is null) return;
                    var req = JsonSerializer.Deserialize(f.Value.Header, ContractsJson.Default.WorkerRequest)!;
                    if (req.Op == "transcribe") await Task.Delay(TranscribeMs);
                    if (req.Op == "ocr" && GarbageOnOcr)
                    {
                        await BinaryFrame.WriteAsync(c, "not json"u8.ToArray(), Array.Empty<byte>(), CancellationToken.None);
                        continue;
                    }
                    var resp = req.Op switch
                    {
                        "hello" => new WorkerResponse(req.Id, true, Hello: new WorkerHello(1, "CPU", "fake", "m", "r", 4, true, true, [])),
                        "transcribe" => new WorkerResponse(req.Id, true, Transcript: new TranscribeResult(true, "hi", "en", [], 3)),
                        _ => new WorkerResponse(req.Id, true, Ocr: new OcrResult([], "fake", 1)),
                    };
                    await BinaryFrame.WriteAsync(c, JsonSerializer.SerializeToUtf8Bytes(resp, ContractsJson.Default.WorkerResponse), Array.Empty<byte>(), CancellationToken.None);
                }
            }
            catch { }
        });
        return pid;
    }

    public void Kill(int pid) { lock (Killed) Killed.Add(pid); }
    public bool HasExited(int pid) => false;
}

public class Sweep3IWorkerTests
{
    [Fact]
    public async Task A2_ocr_timeout_while_its_write_waits_keeps_the_worker_and_the_transcribe()
    {
        var l = new SlowWorkerLauncher();
        await using var sup = new WorkerSupervisor(l, "x", () => 600) { VerifyClientPid = false };
        var client = new WorkerClient(sup);
        await client.PrewarmAsync(CancellationToken.None);
        int pid = sup.Pid;
        var tr = client.TranscribeAsync(new float[16000], "mic", "auto", CancellationToken.None);
        await Task.Delay(300);   // the worker is busy with the transcribe: the OCR frame cannot be written
        using var cts = new CancellationTokenSource(500);   // the keyframe OCR timeout, shortened
        await Assert.ThrowsAnyAsync<OperationCanceledException>(() => client.OcrAsync(new byte[64 * 64 * 4], 64, 64, 256, false, cts.Token));
        var t = await tr.WaitAsync(TimeSpan.FromSeconds(10));
        Assert.Equal("hi", t.Text);
        Assert.Equal(0, sup.Failures);
        Assert.True(sup.IsRunning);
        Assert.Equal(pid, sup.Pid);
        Assert.Empty(l.Killed);
        // The abandoned frame went out once the worker was free; its late reply is dropped and the pipe stays in step.
        var ocr = await client.OcrAsync(new byte[64 * 64 * 4], 64, 64, 256, false, CancellationToken.None).WaitAsync(TimeSpan.FromSeconds(10));
        Assert.Equal("fake", ocr.Engine);
        Assert.Single(l.Started);
    }

    [Fact]
    public async Task A2_broken_frame_kills_that_worker_process()
    {
        var l = new SlowWorkerLauncher { GarbageOnOcr = true };
        await using var sup = new WorkerSupervisor(l, "x", () => 600) { VerifyClientPid = false };
        var client = new WorkerClient(sup);
        await client.PrewarmAsync(CancellationToken.None);
        int pid = sup.Pid;
        await Assert.ThrowsAnyAsync<Exception>(() => client.OcrAsync(new byte[16], 2, 2, 8, false, CancellationToken.None).WaitAsync(TimeSpan.FromSeconds(10)));
        for (int i = 0; i < 50 && l.Killed.Count == 0; i++) await Task.Delay(50);
        lock (l.Killed) Assert.Equal([pid], l.Killed);   // the old worker, not left running next to the next one
        Assert.False(sup.IsRunning);
        Assert.Equal(1, sup.Failures);
    }

    [Fact]
    public void A2_job_limit_struct_has_the_native_size()
        => Assert.Equal(Environment.Is64BitProcess ? 144 : 112, Marshal.SizeOf<KillOnCloseJob.JOBOBJECT_EXTENDED_LIMIT_INFORMATION>());

    [Fact]
    public async Task A2_closing_the_job_kills_its_processes()
    {
        using var job = KillOnCloseJob.TryCreate();
        Assert.NotNull(job);
        var psi = new ProcessStartInfo(Path.Combine(Environment.SystemDirectory, "cmd.exe"), "/c ping -n 30 127.0.0.1 >nul")
        { UseShellExecute = false, CreateNoWindow = true };
        using var p = Process.Start(psi)!;
        try
        {
            Assert.True(job!.Assign(p.Handle));
            Assert.False(p.HasExited);
            job.Dispose();
            await p.WaitForExitAsync().WaitAsync(TimeSpan.FromSeconds(10));
            Assert.True(p.HasExited);
        }
        finally
        {
            if (!p.HasExited) p.Kill(entireProcessTree: true);   // only the process this test started
        }
    }
}
