// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using BetterRecall.Contracts;
using BetterRecall.Core;
using BetterRecall.Core.Ipc;

// betterrecall ipc <words...>: sends one line to the public pipe and prints the response line (research 7.4).
// Exit 0 on ok:true, 1 on ok:false or no response, 2 when the agent is unreachable or untrusted.
// betterrecall ipc purge <scope> [--yes]: preview and confirm on one connection (tokens are per connection); without
// --yes it asks at the console.

if (args.Length < 2 || !args[0].Equals("ipc", StringComparison.OrdinalIgnoreCase))
{
    Console.Error.WriteLine("Usage: betterrecall ipc <words...>");
    return 1;
}

var words = args.Skip(1).ToList();
bool purge = CliPurge.Applies(words);
bool yes = purge && CliPurge.TakeYes(words);
var line = string.Join(' ', words);
var home = DataPaths.Home();
var settings = SettingsStore.ReadOnly(home);
var pipe = PipeNames.Public(settings.Ipc.PipeName, PipeNames.UserSid(), home);   // the agent of this data folder only
var agentExe = InstallLayout.Full(AppContext.BaseDirectory, InstallLayout.AgentExe);

using var cts = new CancellationTokenSource();
Console.CancelKeyPress += (_, e) => { e.Cancel = true; cts.Cancel(); };

var (result, client) = await PipeClient.ConnectAsync(pipe, agentExe, 1000, cts.Token);
if (result == PipeConnectResult.NotRunning) { Console.WriteLine("err BetterRecall not running"); return 2; }
if (result != PipeConnectResult.Ok || client == null) { Console.WriteLine("err BetterRecall not trusted"); return 2; }

await using (client)
{
    if (purge)
    {
        try
        {
            return await CliPurge.RunAsync((l, ct) => client.RequestAsync(l, ct), line, yes,
                interactive: !Console.IsInputRedirected, Console.ReadLine, Console.Out, Console.Error, cts.Token);
        }
        catch (OperationCanceledException) { return 1; }
        catch (IOException) { return 1; }
    }

    string? response;
    try { response = await client.RequestAsync(line, cts.Token); }
    catch (OperationCanceledException) { return 1; }
    catch (IOException) { response = null; }
    if (response == null) return 1;
    Console.WriteLine(response);
    bool ok;
    try { ok = Envelope.Parse(response).Ok; } catch { return 1; }
    if (!ok)
    {
        if (words.Count >= 2 && words[0].Equals("purge", StringComparison.OrdinalIgnoreCase) && words[1].Equals("confirm", StringComparison.OrdinalIgnoreCase))
            Console.Error.WriteLine(CliPurge.HintConfirm);
        return 1;
    }

    // subscribe turns the connection into a stream: print every line until the agent closes it or Ctrl+C.
    if (line.TrimStart().StartsWith("subscribe", StringComparison.OrdinalIgnoreCase))
    {
        try
        {
            while (await client.ReadLineAsync(cts.Token) is { } next) Console.WriteLine(next);
        }
        catch (OperationCanceledException) { }
        catch (IOException) { }
    }
    return 0;
}
