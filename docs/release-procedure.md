# Release procedure

How a BetterRecall release is built, checked and published. Written for 0.4.0-dev. Authenticode signing details are in `docs\release-signing.md`, release-key trust in `docs\key-trust.md`.

## 1. Before the build

1. `main` is green: `python Scripts/dev_build.py --test` and `cargo clippy --workspace --all-targets -- -D warnings` in `rust\`.
2. The version is set in `Directory.Build.props` and `rust\Cargo.toml` (they must match).
3. The architecture docs and `docs\rust-port.md` describe the release commit.
4. Open GitHub issues labelled `release-blocker` are closed.

## 2. Build

```powershell
python Scripts/build_installer.py --pubkey <release public key> --key <release private key> [--sign]
```

The release private key never leaves the release machine and is never committed. `--sign` adds Authenticode signing once a code-signing certificate exists (1.0.0 and later).

Output in `artifacts\release\`: `BetterRecall-Setup-<version>.exe`, its notices file, `update.json` and `update.sig`.

## 3. Test

1. Windows Sandbox install test: `python Scripts/sandbox_installer_test.py` (fresh install, upgrade from the previous release, Modify, uninstall with and without data).
2. With `--sign`: `Scripts\sandbox_sign_test.py` in the sandbox.
3. Start the installed app in the sandbox, run a search, save a replay, open every settings page.

## 4. VirusTotal

Every release is checked before it is published:

```powershell
$env:VT_API_KEY = "<your key>"   # from your VirusTotal account; never commit it
python Scripts/virustotal_check.py --installer artifacts\release\BetterRecall-Setup-<version>.exe --layout artifacts\release\layout
```

- The script first looks files up by SHA-256 only. Files VirusTotal has never seen are listed as `unknown`.
- `--upload` sends unknown files for analysis. An upload makes the file public on VirusTotal, so upload only the final build you are about to publish.
- Exit 0: no engine flags anything. Exit 1: detections. Exit 2: files still unknown or pending.

Release rule: publish only when no major engine (Microsoft, Kaspersky, BitDefender, ESET, Avast/AVG, Sophos, Symantec, Trend Micro, McAfee, CrowdStrike, SentinelOne, Malwarebytes, Google) flags a file. A detection by another engine is allowed only when it is written down in the release notes and reported to that vendor as a false positive (most vendors have a web form; attach the VirusTotal link). Keep the `--json` output with the release files.

Unsigned builds are flagged more often by heuristic engines; that is one reason for Authenticode signing at 1.0.0.

## 5. Publish

1. Tag `v<version>` on the release commit and push the tag.
2. Create the GitHub release from the tag. Attach the installer, its notices file, `update.json` and `update.sig`. The agent's update check only accepts releases under `https://github.com/lnminh1411/BetterRecall/releases/`.
3. Release notes: what changed, known issues, the VirusTotal links, and any documented detections.

## 6. After release

- Triage new GitHub issues daily for the first week (labels: `bug`, `enhancement`, `question`, `needs-info`, `release-blocker`; security reports go through private advisories, see `SECURITY.md`).
- If a release is broken, publish a fixed version rather than deleting the release: installed copies only ever move forward.
