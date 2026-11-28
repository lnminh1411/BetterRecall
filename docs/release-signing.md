# Release Authenticode signing

BetterRecall ships as a self-extracting Windows installer (`BetterRecall-Setup-<version>.exe`: PE stub + payload zip + 64-byte `BRSETUP1` trailer) plus per-user binaries in the install folder. Authenticode signing is planned for 1.0.0; this document covers the certificate options, the signing order in `Scripts/build_installer.py`, how Authenticode interacts with our own release key, and how to test signed installers offline in Windows Sandbox.

## Certificate routes

| Route | Typical cost | SmartScreen reputation | Key storage / hardware |
|---|---|---|---|
| **OV** (Organization Validation) | ~$200-400/yr | Starts at zero; warns users until enough downloads build reputation | FIPS 140-2 USB hardware token (SafeNet/YubiKey) or cloud HSM (CA/B Forum requirement since June 2023) |
| **EV** (Extended Validation) | ~$400-700/yr | Immediate or much faster SmartScreen trust | Hardware token or cloud HSM (same physical presence/PIN requirement) |
| **Azure Trusted Signing** (Microsoft) | ~$10/mo (Basic) | Strong SmartScreen reputation out of the box (Microsoft root, short-lived 3-day certs with RFC 3161 timestamps) | Cloud-managed in Azure; no physical token; requires `BR_SIGN_DLIB` + `BR_SIGN_METADATA` (`signtool /dlib /dmdf`) |

No certificate or secret lives in the repository. `Scripts/build_installer.py --sign` reads certificate selection exclusively from environment variables:
- Store certificate (OV/EV or test cert): `BR_SIGN_THUMBPRINT` (`signtool sign /fd SHA256 /sha1 <thumbprint>`)
- Azure Trusted Signing: `BR_SIGN_DLIB` + `BR_SIGN_METADATA` (`signtool sign /fd SHA256 /dlib <dll> /dmdf <json>`)
- Timestamp server: `BR_SIGN_TIMESTAMP` (default `http://timestamp.acs.microsoft.com`, omitted with `--sign-no-timestamp`)
- Optional tool path override: `BR_SIGNTOOL` (otherwise the newest `C:\Program Files (x86)\Windows Kits\10\bin\*\x64\signtool.exe` is used)

## Signing order (`Scripts/build_installer.py --sign`)

The order matters because `integrity.json` hashes every shipped file in the layout and `update.json` hashes the final packed installer:

1. **Sign our own layout binaries** (after the layout is final, before `br-sign manifest`): sign every `.exe` and `.dll` in the layout that is not already Authenticode-signed (`BetterRecall.Agent.exe`, `BetterRecall.Worker.exe`, `betterrecall.exe`, `betterrecall-mcp.exe`, the UI executable(s) and our own DLLs). Files that already pass `signtool verify /pa` are skipped so vendor signatures stay intact; unsigned third-party DLLs do get our signature, so check the list `build_installer.py` prints before a release. **`BetterRecall-Setup.exe` is intentionally NOT signed here** (see below).
2. **`br-sign manifest` and `br-sign verify`**: `build_installer.py` first copies the unsigned `br-installer.exe` stub into the layout as `BetterRecall-Setup.exe` (br-sign does not; running `br-sign manifest` by hand without that copy ships an `integrity.json` without the stub hash). It also refuses a stub whose CheckSum or certificate table is not zero. `br-sign manifest` then hashes all layout files (now including the Authenticode-signed app binaries and the unsigned stub) into `integrity.json`, signs `integrity.sig`, and verifies the layout.
3. **`br-sign pack`**: concatenates the unsigned stub + payload zip (excluding `BetterRecall-Setup.exe`) + 64-byte `BRSETUP1` trailer into `BetterRecall-Setup-<version>.exe`.
4. **Sign the packed installer**: runs `signtool sign` on `BetterRecall-Setup-<version>.exe`. Authenticode sets the PE optional header `CheckSum`, populates `IMAGE_DIRECTORY_ENTRY_SECURITY` (data directory 4), pads to 8-byte alignment with 0..7 zero bytes, and appends the certificate table at EOF. `br-installer`'s `read_trailer` and `verify` locate the `BRSETUP1` trailer immediately before that certificate table.
5. **`br-sign update`**: hashes the final, Authenticode-signed `BetterRecall-Setup-<version>.exe` into `update.json` and signs `update.sig`.

Without `--sign`, `build_installer.py` skips steps 1 and 4 and behaves unchanged.

## Why the installed `BetterRecall-Setup.exe` stays unsigned (and 1.0 follow-up)

During install, `copy_stub_exe` extracts the first `trailer.offset` bytes of the running installer to `<install>\BetterRecall-Setup.exe` (used later for Modify and Uninstall), and `br_integrity::check_single_file` checks its SHA-256 against `integrity.json`. Because Authenticode signing in step 4 modifies the PE optional header `CheckSum` and data directory 4 (`IMAGE_DIRECTORY_ENTRY_SECURITY`) of the packed installer, `copy_stub_exe` zeroes those two header fields in the copied stub so it is byte-identical to the unsigned stub hashed in step 2.

That means the installed `<install>\BetterRecall-Setup.exe` is unsigned in 0.4, which is acceptable because Modify/Uninstall run from an already-installed local folder.

**1.0 follow-up**: sign `br-installer.exe` *before* step 2 (so `integrity.json` hashes a signed stub), record its original `CheckSum` and `IMAGE_DIRECTORY_ENTRY_SECURITY` values (or embed them in the trailer), and restore them when copying the stub—while also copying the stub's own certificate table—so the installed `BetterRecall-Setup.exe` retains a valid Authenticode signature.

## Relation to the release key (`docs/key-trust.md`)

Authenticode and BetterRecall's ECDSA P-256 release key serve different layers and complement each other:
- **Authenticode (X.509 / Windows CA)** establishes OS-level publisher identity and SmartScreen reputation when a user downloads and runs `BetterRecall-Setup-<version>.exe` or Windows inspects our binaries.
- **Release key (`integrity.json` / `update.json`, see `docs/key-trust.md`)** provides BetterRecall's own offline file-by-file tamper check at startup and authenticated auto-updates, without depending on online CA/OCSP checks or trusting any external CA.

## Running the offline Windows Sandbox signing test

`Scripts/sandbox_sign_test.py` tests the full Authenticode-signed installer path (self-signed cert creation, `signtool sign` without timestamp, `signtool verify /pa`, silent install with `/S /models=later /norun --dev-root ...`, verification that `<install>\BetterRecall-Setup.exe` matches `integrity.json` or the unsigned stub prefix, and silent `/uninstall /S`).

It refuses to run unless `USERNAME=WDAGUtilityAccount` so it can never modify the host certificate store.

Inside Windows Sandbox (networking disabled, with the unsigned installer and Windows Kits `bin` folder mapped read-only):

```powershell
python C:\BRTest\in\sandbox_sign_test.py `
  --installer C:\BRTest\in\BetterRecall-Setup-0.4.0-dev.exe `
  --kits-bin C:\BRTest\kits `
  --out C:\BRTest\results\sign_results.json
```

Check `sign_results.json` for `"ok": true` and per-step status.
