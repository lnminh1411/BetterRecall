# SPDX-License-Identifier: AGPL-3.0-only
# Copyright (c) BetterRecall contributors
r"""Prepare and optionally run the Windows Sandbox installer test suite (stage I6b).

Usage:
  python Scripts/sandbox_installer_test.py [--prepare] [--v1 PATH] [--v2 PATH] [--build] [--work-dir DIR]
  python Scripts/sandbox_installer_test.py --run [--work-dir DIR] [--timeout SECONDS]

Default mode is --prepare (stages files, writes sandbox.wsb and run_tests.ps1; does not launch Windows Sandbox).
--run starts Windows Sandbox headless via `wsb`, executes run_tests.ps1 inside it, polls results\results.json, and stops the sandbox.
"""
import argparse
import json
import shutil
import subprocess
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
DEFAULT_WORK_DIR = Path(r"K:\scratch\br-sandbox-install")
V1_VERSION = "0.3.0-dev.1"
V2_VERSION = "0.3.0-dev.2"

SANDBOX_PS1 = r"""$ErrorActionPreference = 'Stop'
$resultsDir = 'C:\BRTest\results'
$inDir = 'C:\BRTest\in'
$logFile = Join-Path $resultsDir 'log.txt'
$resultsFile = Join-Path $resultsDir 'results.json'
New-Item -ItemType Directory -Force -Path $resultsDir | Out-Null

$steps = [System.Collections.Generic.List[object]]::new()
function Log([string]$msg) {
    $line = "[$(Get-Date -Format 'HH:mm:ss')] $msg"
    Add-Content -LiteralPath $logFile -Value $line -Encoding UTF8
}
function Record([string]$name, [bool]$ok, [string]$detail = '') {
    Log ("{0}: {1} {2}" -f $name, $(if ($ok) { 'PASS' } else { 'FAIL' }), $detail)
    $steps.Add([pscustomobject]@{ name = $name; ok = $ok; detail = $detail })
    if (-not $ok) { throw "Step failed: $name -- $detail" }
}

$homeDir = Join-Path $env:LOCALAPPDATA 'BetterRecall'
$installDir = Join-Path $env:LOCALAPPDATA 'Programs\BetterRecall'
$binDir = Join-Path $homeDir 'app\bin'
$relayExe = Join-Path $binDir 'betterrecall-mcp.exe'
$cliShim = Join-Path $binDir 'betterrecall.cmd'
$startMenu = Join-Path $env:APPDATA 'Microsoft\Windows\Start Menu\Programs\BetterRecall.lnk'
$uninstallKey = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\BetterRecall'
$runKey = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run'
$appKey = 'HKCU:\Software\BetterRecall'
$v1Exe = Join-Path $inDir 'installer-v1.exe'
$v2Exe = Join-Path $inDir 'installer-v2.exe'

$relayProc = $null
$overallOk = $false
$errorMsg = ''

function Wait-UiPipe([int]$timeoutSec = 30) {
    $sid = [System.Security.Principal.WindowsIdentity]::GetCurrent().User.Value
    $pipeName = "BetterRecall.ui.$sid"
    $deadline = (Get-Date).AddSeconds($timeoutSec)
    while ((Get-Date) -lt $deadline) {
        if ([System.IO.Directory]::GetFiles('\\.\pipe\', $pipeName).Length -gt 0) {
            return $true
        }
        Start-Sleep -Milliseconds 250
    }
    return $false
}

function Read-RelayLine($proc, [int]$timeoutMs = 60000) {
    # A relay that stops answering must fail the step, not hang the script (results.json is written in finally).
    $task = $proc.StandardOutput.ReadLineAsync()
    if ($task.Wait($timeoutMs)) { return $task.Result }
    return $null
}

try {
    Log "Starting sandbox installer verification"

    # Step 5 pre-seed: fake launcher-era install and user settings before step 1.
    $legacyVer = Join-Path $homeDir 'app\0.1.0-0123456789ab'
    New-Item -ItemType Directory -Force -Path $legacyVer, $binDir | Out-Null
    Set-Content -LiteralPath (Join-Path $legacyVer 'BetterRecall.Agent.exe') -Value 'legacy' -Encoding ASCII
    Set-Content -LiteralPath (Join-Path $binDir 'BetterRecall.exe') -Value 'legacy-stub' -Encoding ASCII
    Set-Content -LiteralPath (Join-Path $binDir 'current') -Value $legacyVer -Encoding ASCII
    $settingsPath = Join-Path $homeDir 'settings.json'
    Set-Content -LiteralPath $settingsPath -Value '{}' -Encoding UTF8
    Record 'preseed_launcher_era' $true 'seeded 0.1.0-0123456789ab, bin\BetterRecall.exe, bin\current, settings.json'

    # Step 1: silent install of .1
    $p1 = Start-Process -FilePath $v1Exe -ArgumentList '/S', '/models=later', '/norun' -Wait -PassThru -NoNewWindow
    Record 'install_v1_exit' ($p1.ExitCode -eq 0) "exit=$($p1.ExitCode)"

    $expectedFiles = @(
        'BetterRecall.Agent.exe',
        'betterrecall.exe',
        'betterrecall-mcp.exe',
        'BetterRecall-Setup.exe',
        'worker\BetterRecall.Worker.exe',
        'ui\BetterRecall.UI.exe',
        '.install'
    )
    $missing = $expectedFiles | Where-Object { -not (Test-Path -LiteralPath (Join-Path $installDir $_)) }
    Record 'install_v1_files' ($missing.Count -eq 0) ("missing=" + ($missing -join ','))

    $dispVer = (Get-ItemProperty -LiteralPath $uninstallKey).DisplayVersion
    $instPath = (Get-ItemProperty -LiteralPath $appKey).InstallPath
    $runVal = (Get-ItemProperty -LiteralPath $runKey).BetterRecall
    $expectedRun = '"' + (Join-Path $installDir 'BetterRecall.Agent.exe') + '"'
    Record 'install_v1_registry' (
        ($dispVer -eq '0.3.0-dev.1') -and
        ($instPath -ieq $installDir) -and
        ($runVal -ieq $expectedRun)
    ) "ver=$dispVer installPath=$instPath run=$runVal"

    Record 'install_v1_shortcut' (Test-Path -LiteralPath $startMenu) $startMenu
    Record 'install_v1_shims' ((Test-Path -LiteralPath $relayExe) -and (Test-Path -LiteralPath $cliShim)) "$relayExe ; $cliShim"

    # Step 5 verify: launcher-era leftovers removed, settings.json preserved
    $cleanupOk = (-not (Test-Path -LiteralPath $legacyVer)) -and
                 (-not (Test-Path -LiteralPath (Join-Path $binDir 'BetterRecall.exe'))) -and
                 (-not (Test-Path -LiteralPath (Join-Path $binDir 'current'))) -and
                 (Test-Path -LiteralPath $settingsPath)
    Record 'launcher_cleanup' $cleanupOk 'old version folder and stub removed, settings.json kept'

    # Step 2: start agent, wait for UI pipe, test CLI shim (expect exit 3) and MCP relay initialize + tools/list
    $agentExe = Join-Path $installDir 'BetterRecall.Agent.exe'
    Start-Process -FilePath $agentExe -WorkingDirectory $installDir -WindowStyle Hidden
    $pipeReady = Wait-UiPipe 30
    Record 'agent_ui_pipe' $pipeReady 'waited for UI pipe'

    $cliOut = & cmd.exe /c "`"$cliShim`" ipc ping" 2>&1
    $cliCode = $LASTEXITCODE
    Record 'cli_shim_ipc_ping' ($cliCode -eq 3) "exit=$cliCode out=$($cliOut -join ' ')"

    # Start persistent MCP relay session from <home>\app\bin
    $psi = [System.Diagnostics.ProcessStartInfo]::new($relayExe)
    $psi.WorkingDirectory = $binDir
    $psi.UseShellExecute = $false
    $psi.CreateNoWindow = $true
    $psi.RedirectStandardInput = $true
    $psi.RedirectStandardOutput = $true
    $psi.RedirectStandardError = $true
    $relayProc = [System.Diagnostics.Process]::Start($psi)

    $relayProc.StandardInput.WriteLine('{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"sandbox","version":"1"}}}')
    $relayProc.StandardInput.Flush()
    $initReply = (Read-RelayLine $relayProc)
    Record 'mcp_relay_initialize' ($null -ne $initReply -and $initReply.Contains('"id":1') -and $initReply.Contains('"result"')) $initReply

    $relayProc.StandardInput.WriteLine('{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}')
    $relayProc.StandardInput.Flush()
    $listReply = (Read-RelayLine $relayProc)
    Record 'mcp_relay_tools_list' ($null -ne $listReply -and $listReply.Contains('"id":2') -and $listReply.Contains('"tools"')) "len=$($listReply.Length)"

    # Step 3: silent upgrade to .2 while relay stays open
    $p2 = Start-Process -FilePath $v2Exe -ArgumentList '/S' -Wait -PassThru -NoNewWindow
    Record 'upgrade_v2_exit' ($p2.ExitCode -eq 0) "exit=$($p2.ExitCode)"
    $pipeReady2 = Wait-UiPipe 30
    Record 'upgrade_v2_agent_restarted' $pipeReady2 'agent restarted after upgrade'

    $dispVer2 = (Get-ItemProperty -LiteralPath $uninstallKey).DisplayVersion
    Record 'upgrade_v2_version' ($dispVer2 -eq '0.3.0-dev.2') "ver=$dispVer2"

    $relayProc.StandardInput.WriteLine('{"jsonrpc":"2.0","id":3,"method":"tools/list","params":{}}')
    $relayProc.StandardInput.Flush()
    $postUpgradeReply = (Read-RelayLine $relayProc)
    Record 'mcp_relay_survives_upgrade' ($null -ne $postUpgradeReply -and $postUpgradeReply.Contains('"id":3') -and $postUpgradeReply.Contains('"tools"')) "len=$($postUpgradeReply.Length)"

    $relayProc.StandardInput.Close()
    $relayProc.WaitForExit(5000) | Out-Null
    $relayProc.Dispose()
    $relayProc = $null

    # Step 4: downgrade .1 over .2 without /allowdowngrade -> exit 4, nothing changed
    $pDown = Start-Process -FilePath $v1Exe -ArgumentList '/S', '/norun' -Wait -PassThru -NoNewWindow
    $dispVerAfterDown = (Get-ItemProperty -LiteralPath $uninstallKey).DisplayVersion
    Record 'downgrade_refused' (($pDown.ExitCode -eq 4) -and ($dispVerAfterDown -eq '0.3.0-dev.2')) "exit=$($pDown.ExitCode) ver=$dispVerAfterDown"

    # Step 6: silent uninstall (relaunches from temp, so wait for install folder removal)
    $setupInstalled = Join-Path $installDir 'BetterRecall-Setup.exe'
    $pUn = Start-Process -FilePath $setupInstalled -ArgumentList '/uninstall', '/S' -Wait -PassThru -NoNewWindow
    Record 'uninstall_start' ($pUn.ExitCode -eq 0) "exit=$($pUn.ExitCode)"

    $deadline = (Get-Date).AddSeconds(30)
    while ((Test-Path -LiteralPath $installDir) -and ((Get-Date) -lt $deadline)) {
        Start-Sleep -Milliseconds 300
    }
    $uninstallGone = (-not (Test-Path -LiteralPath $installDir)) -and
                     (-not (Test-Path -LiteralPath $uninstallKey)) -and
                     ($null -eq (Get-ItemProperty -LiteralPath $runKey -Name 'BetterRecall' -ErrorAction SilentlyContinue)) -and
                     ($null -eq (Get-ItemProperty -LiteralPath $appKey -Name 'InstallPath' -ErrorAction SilentlyContinue)) -and
                     (-not (Test-Path -LiteralPath $startMenu)) -and
                     (-not (Test-Path -LiteralPath $relayExe)) -and
                     (-not (Test-Path -LiteralPath $cliShim))
    Record 'uninstall_clean' $uninstallGone 'install dir, registry, shortcut, and shims removed'
    Record 'uninstall_keeps_data' (Test-Path -LiteralPath $settingsPath) $settingsPath

    $overallOk = $true
} catch {
    $errorMsg = $_.Exception.Message
    Log "ERROR: $errorMsg"
} finally {
    if ($null -ne $relayProc) {
        try { if (-not $relayProc.HasExited) { $relayProc.Kill() } } catch {}
        $relayProc.Dispose()
    }
    $summary = [pscustomobject]@{
        ok = $overallOk
        error = $errorMsg
        steps = $steps
    }
    $summary | ConvertTo-Json -Depth 5 | Set-Content -LiteralPath $resultsFile -Encoding UTF8
    Log "Finished (ok=$overallOk)"
}
"""


def write_wsb_config(wsb_path: Path, in_dir: Path, results_dir: Path) -> None:
    xml = f"""<Configuration>
  <VGpu>Default</VGpu>
  <Networking>Disable</Networking>
  <MemoryInMB>8192</MemoryInMB>
  <MappedFolders>
    <MappedFolder>
      <HostFolder>{in_dir}</HostFolder>
      <SandboxFolder>C:\\BRTest\\in</SandboxFolder>
      <ReadOnly>true</ReadOnly>
    </MappedFolder>
    <MappedFolder>
      <HostFolder>{results_dir}</HostFolder>
      <SandboxFolder>C:\\BRTest\\results</SandboxFolder>
      <ReadOnly>false</ReadOnly>
    </MappedFolder>
  </MappedFolders>
</Configuration>
"""
    wsb_path.write_text(xml, encoding="utf-8")


def build_two_installers(work_dir: Path) -> tuple[Path, Path]:
    out_v1 = work_dir / "build-v1"
    out_v2 = work_dir / "build-v2"
    script = ROOT / "Scripts" / "build_installer.py"
    for ver, out_d in ((V1_VERSION, out_v1), (V2_VERSION, out_v2)):
        cmd = [sys.executable, str(script), "--payload-version", ver, "--out", str(out_d)]
        print(">", " ".join(cmd), flush=True)
        r = subprocess.run(cmd, cwd=ROOT)
        if r.returncode != 0:
            raise SystemExit(f"sandbox_installer_test: build_installer.py failed for {ver}")
    return (
        out_v1 / f"BetterRecall-Setup-{V1_VERSION}.exe",
        out_v2 / f"BetterRecall-Setup-{V2_VERSION}.exe",
    )


def prepare(work_dir: Path, v1: Path | None, v2: Path | None, do_build: bool) -> Path:
    in_dir = work_dir / "in"
    results_dir = work_dir / "results"
    in_dir.mkdir(parents=True, exist_ok=True)
    if results_dir.exists():
        shutil.rmtree(results_dir)
    results_dir.mkdir(parents=True, exist_ok=True)

    if do_build:
        v1, v2 = build_two_installers(work_dir)

    if v1 is not None:
        if not v1.is_file():
            raise SystemExit(f"sandbox_installer_test: --v1 not found: {v1}")
        shutil.copy2(v1, in_dir / "installer-v1.exe")
    if v2 is not None:
        if not v2.is_file():
            raise SystemExit(f"sandbox_installer_test: --v2 not found: {v2}")
        shutil.copy2(v2, in_dir / "installer-v2.exe")

    ps1_path = in_dir / "run_tests.ps1"
    ps1_path.write_text(SANDBOX_PS1, encoding="utf-8")

    wsb_path = work_dir / "sandbox.wsb"
    write_wsb_config(wsb_path, in_dir, results_dir)

    print(f"prepared sandbox test in {work_dir}")
    print(f"  wsb:     {wsb_path}")
    print(f"  script:  {ps1_path}")
    print(f"  results: {results_dir}")
    if not (in_dir / "installer-v1.exe").is_file() or not (in_dir / "installer-v2.exe").is_file():
        print("  note:    installer-v1.exe and/or installer-v2.exe not yet staged (pass --v1/--v2 or --build)")
    return wsb_path


def run_sandbox(work_dir: Path, timeout_s: int) -> None:
    wsb_path = work_dir / "sandbox.wsb"
    in_dir = work_dir / "in"
    results_dir = work_dir / "results"
    results_file = results_dir / "results.json"

    for req in (wsb_path, in_dir / "installer-v1.exe", in_dir / "installer-v2.exe", in_dir / "run_tests.ps1"):
        if not req.is_file():
            raise SystemExit(f"sandbox_installer_test: missing staged file: {req} (run --prepare first)")

    if results_file.exists():
        results_file.unlink()

    flags = getattr(subprocess, "CREATE_NO_WINDOW", 0)
    start = subprocess.run(
        # -c takes the configuration XML itself, not a path to a .wsb file.
        ["wsb", "start", "--raw", "-c", wsb_path.read_text(encoding="utf-8")],
        capture_output=True,
        text=True,
        creationflags=flags,
    )
    if start.returncode != 0:
        raise SystemExit(f"wsb start failed ({start.returncode}): {start.stderr.strip() or start.stdout.strip()}")

    try:
        info = json.loads(start.stdout)
        sandbox_id = info.get("Id") or info.get("id")
    except json.JSONDecodeError:
        sandbox_id = start.stdout.strip()
    if not sandbox_id:
        raise SystemExit(f"wsb start returned no sandbox id: {start.stdout!r}")

    print(f"sandbox started: {sandbox_id}", flush=True)
    try:
        # connect opens the remote session window and may not return while it is open: do not wait on it.
        # exec with ExistingLogin fails until that session has logged in, so retry it.
        subprocess.Popen(
            ["wsb", "connect", "--id", sandbox_id],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            creationflags=flags,
        )
        exec_cmd = [
            "wsb",
            "exec",
            "--id",
            sandbox_id,
            "-r",
            "ExistingLogin",
            "-c",
            r"powershell.exe -NoProfile -ExecutionPolicy Bypass -File C:\BRTest\in\run_tests.ps1",
        ]
        login_deadline = time.monotonic() + 180
        while True:
            proc = subprocess.Popen(exec_cmd, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, creationflags=flags)
            try:
                out, err = proc.communicate(timeout=15)
            except subprocess.TimeoutExpired:
                break  # still running inside the sandbox: the test script started
            if proc.returncode == 0:
                break
            if time.monotonic() > login_deadline:
                raise SystemExit(f"wsb exec failed ({proc.returncode}): {(err or out).strip()}")
            time.sleep(5.0)

        deadline = time.monotonic() + timeout_s
        while time.monotonic() < deadline:
            if results_file.is_file():
                break
            time.sleep(2.0)

        if not results_file.is_file():
            raise SystemExit(f"sandbox_installer_test: timed out after {timeout_s}s waiting for {results_file}")

        data = json.loads(results_file.read_text(encoding="utf-8-sig"))
        print(json.dumps(data, indent=2))
        if not data.get("ok"):
            raise SystemExit(f"sandbox_installer_test: FAILED: {data.get('error')}")
    finally:
        subprocess.run(
            ["wsb", "stop", "--id", sandbox_id],
            capture_output=True,
            text=True,
            creationflags=flags,
        )


def main() -> None:
    ap = argparse.ArgumentParser(description="Prepare or run the Windows Sandbox installer test.")
    ap.add_argument("--prepare", action="store_true", help="stage files, .wsb, and PowerShell script (default)")
    ap.add_argument("--run", action="store_true", help="launch Windows Sandbox via wsb and run the staged test")
    ap.add_argument("--build", action="store_true", help="build .1 and .2 installers via build_installer.py during --prepare")
    ap.add_argument("--v1", default="", help="path to 0.3.0-dev.1 installer exe")
    ap.add_argument("--v2", default="", help="path to 0.3.0-dev.2 installer exe")
    ap.add_argument("--work-dir", default=str(DEFAULT_WORK_DIR), help=f"working folder (default: {DEFAULT_WORK_DIR})")
    ap.add_argument("--timeout", type=int, default=300, help="seconds to wait for sandbox results")
    a = ap.parse_args()

    work_dir = Path(a.work_dir).resolve()
    v1 = Path(a.v1).resolve() if a.v1 else None
    v2 = Path(a.v2).resolve() if a.v2 else None

    if a.prepare or not a.run:
        prepare(work_dir, v1, v2, a.build)
    if a.run:
        run_sandbox(work_dir, a.timeout)


if __name__ == "__main__":
    main()
