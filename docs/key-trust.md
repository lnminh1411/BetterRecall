# Release key trust: rotation, revocation and remote update

Written 2026-11-22. Status: plan, not implemented. Decision R10 in `docs\rust-port.md`; milestones K1 to K5 there.

## Why

R9 and R8 rest on one ECDSA P-256 key. Its public half is compiled into every program (`BR_RELEASE_PUBKEY`); its private half signs `integrity.json` and `update.json`. That has three failure modes:

| Event | What happens today |
|---|---|
| Private key stolen | The thief can sign updates that every install with auto-update on downloads and runs. Nothing can tell installs to stop trusting the key. |
| Private key lost | No further update can be signed. Every user has to download a new installer by hand, and the old installs keep refusing anything not signed by the lost key. |
| Planned change (new hardware, a second maintainer, a weak key) | Same as lost: only a manual reinstall moves an install to the new key. |

No signed release exists yet: the key has not been generated and the 0.3.0-dev installers are unsigned. So the trust format can still change without breaking any installed copy. This plan should land before the first signed release.

## Design

The design follows the root-of-trust idea of The Update Framework (TUF): a small set of rarely used root keys signs a list of the keys that may sign releases. It keeps only the parts a single-maintainer desktop app needs.

### Two kinds of keys

| Key | Count | Where it lives | Signs | Used |
|---|---|---|---|---|
| Root | 3, any 2 must sign (threshold 2) | Offline, in three separate places (for example a hardware security key, an encrypted file on an offline machine, a printed paper backup in a safe) | `trust.json` only | A few times a year at most: key rotation, revocation |
| Release | 1 active (2 during a rotation overlap) | The maintainer's signing machine, later a hardware security key (K4) | `integrity.json`, `update.json` | Every release |

Losing one root key or having one stolen changes nothing by itself: two signatures are needed. The threshold and the key count are recorded in `trust.json`, so they can change later.

### The trust file

`trust.json` (UTF-8, no BOM, LF, keys sorted, same canonical form as `integrity.json`):

```json
{
  "format": 1,
  "version": 3,
  "expires": "2027-11-22T00:00:00Z",
  "root": { "threshold": 2, "keys": ["<keyid>", "<keyid>", "<keyid>"] },
  "keys": {
    "<keyid>": { "role": "root",    "public": "<SPKI PEM>" },
    "<keyid>": { "role": "release", "public": "<SPKI PEM>", "status": "active",  "since": 3 },
    "<keyid>": { "role": "release", "public": "<SPKI PEM>", "status": "revoked", "since": 1, "revoked": 3, "reason": "compromised" }
  }
}
```

- `keyid` is the lowercase hex SHA-256 of the key's SPKI DER, the fingerprint `br-sign keygen` already prints.
- `version` only goes up. An install never accepts a lower or equal version than the highest it has verified (no rollback to a file that still trusts a revoked key).
- `expires` limits how long an attacker who blocks updates can keep an install on old trust (freeze attack). Expiry never stops the app from running; it only stops it from accepting updates until a newer trust file is fetched. Each new trust file is published well before the old one expires (yearly, or with every rotation).
- `trust.sig.json` holds the signatures: `{"signatures":[{"keyid":"...","sig":"<base64 P1363>"}]}`. A trust file is valid when at least `root.threshold` distinct root keys from the *previous* version sign it, and, when the root set changes, also `threshold` keys of the *new* root set (so a new root set has to prove it holds its keys).
- `integrity.sig` and `update.sig` change from raw 64 bytes to the same JSON form with one entry, so the checker knows which release key signed and can tell an unknown key from a revoked one.

### What each program embeds

Instead of the release public key, builds embed the first trust file, version 1 (`BR_TRUST_ROOT` in Rust, the `BetterRecallTrustRoot` MSBuild property in C#, both pointing at `trust-1.json` and its signature file). It contains the root keys and the first release key. Root keys change rarely, so a build stays valid across many release-key rotations without rebuilding anything.

### Where newer trust files come from

Through the Update switch, so there is no new setting and no new network path (R8 unchanged):
- Every update check (the user pressing **Check for updates**, or the daily check when **Install updates automatically** is on) first fetches the trust chain, then the release feed.
- Trust files are assets of one GitHub release with the fixed tag `trust`: `trust-2.json`, `trust-2.sig.json`, `trust-3.json`, and so on. The client walks from the highest version it has verified, one version at a time, until the next file is missing, verifying each step against the previous one. Asset URLs are pinned to `https://github.com/lnminh1411/BetterRecall/releases/download/trust/`, like the update assets.
- The highest verified trust file is stored in `<home>\cache\updates\trust.json` (+ signature). On every start, programs take the newer of the embedded chain and the stored file, re-verifying the stored file's chain from the embedded version 1. A stored file that fails verification is ignored and logged.
- An install with the Update switch off that never presses Check never learns about a revocation. That is the price of R8's "no network unless the user asks", and the Update page says so next to the switch.

### Checks with trust in place

| Check | Accepts | Rejects |
|---|---|---|
| Startup integrity (R9) | `integrity.json` signed by a release key that is `active` or `retired` in the current trust | Unknown key or bad signature: stop, as today (exit 86). Revoked key: start anyway, but show "BetterRecall's installed files are signed with a key that has been withdrawn. Update BetterRecall." in the tray and on the Update page, and treat any update as urgent |
| Update (R8) | `update.json` signed by an `active` release key, version newer than installed (as today), from a trust file that has not expired | Revoked, unknown or expired-trust signatures, with a reason the Update page shows |
| Installer payload | Payload `integrity.json` signed by an `active` or `retired` key in the newer of the installer's embedded trust and the trust shipped in the payload (`trust-N.json` sits in the layout) | Same as startup, but a revoked key stops the install |
| Trust update | A higher version that chains from the current one with enough root signatures, not expired, `format` known | Anything else; the stored trust is left as it was |

Why a revoked key does not stop the app from starting: signatures carry no trustworthy time, so a revoked key cannot tell "signed before the theft" from "signed by the thief". Stopping every install signed with it would brick all of them at once, including the ones that are fine. Whoever can write the install folder can already patch the checkers (R9's limit), so refusing to start buys little. Refusing *updates* from that key is what stops the real attack.

### Procedures

**Planned release-key rotation** (yearly, or when moving to a hardware key):
1. `br-sign keygen --role release` on the signing machine, giving key B.
2. Root ceremony: `br-sign trust next --add-release B` writes `trust-(N+1).json` with A and B both `active`. Two root keys sign it (`br-sign trust sign --key ...`, run once per root key, adding to `trust-(N+1).sig.json`).
3. Publish the trust file and signature as assets of the `trust` release. Sign the next release with B.
4. After every supported install had a chance to fetch it (a few months), the next trust file marks A `retired`: no new signatures are accepted from it, but existing installs signed with A still start without a warning.

**Compromised release key**: same as rotation, but A is marked `revoked` in the same trust file that adds B, and a new release signed with B is published at once. Installs pick up both on their next update check.

**Lost release key**: rotation without the overlap. Nothing is revoked unless theft is possible.

**Lost or stolen root key**: the two remaining root keys sign a trust file that replaces it with a new third root key (threshold still 2). If two root keys are lost, the trust chain cannot continue: publish a new build with a new `trust-1.json` and tell users to reinstall by hand. The three separate storage places exist to make this case unlikely.

**Trust renewal**: before `expires`, two root keys sign the next version with only a new `expires`.

Every procedure is a short checklist in `rust\crates\br-sign\README.md`, run on an offline machine for root keys. The private root keys never touch the signing or build machine.

## Not in scope

- X.509 certificates, Authenticode or a CA: Windows code signing is a separate question (SmartScreen), and it does not cover R9's file-by-file check.
- Full TUF (snapshot and timestamp roles, delegations): one release feed and one maintainer do not need them. `expires` plus the monotonic version cover rollback and freeze for the trust file. The release feed itself is already protected by the "only newer versions" rule.
- Online revocation status (OCSP-like): it would need a network request on every start, which R8 rules out.

## Milestones

| Stage | Scope | Done when |
|---|---|---|
| K1 | `br-trust` crate: `trust.json` and `*.sig.json` formats, canonical bytes, chain verification (threshold, root-set change, monotonic version, expiry), key status lookup; `br-sign` commands `keygen --role`, `trust init`, `trust next`, `trust sign`, `trust verify`; golden cross-language fixtures (a 3-root, threshold-2 test chain) | Rust tests: valid chain, too few signatures, signature from a removed root key, rollback, expired, revoked and retired release keys, root-set change signed by old set only; C# test verifies the same fixtures |
| K2 | `br-integrity` and the C# `IntegrityCheck` switch from one key to trust: embedded `trust-1.json` (`BR_TRUST_ROOT`, `BetterRecallTrustRoot`), stored-trust merge at startup, the revoked-key warning state, installer payload check; `integrity.sig`/`update.sig` in the JSON form; `build_installer.py` ships `trust-N.json` in the layout | tests: startup with active, retired, revoked, unknown key; stored trust newer than embedded; tampered stored trust ignored; installer refuses a revoked payload key |
| K3 | Updater fetches the trust chain before the release feed on every check (manual or automatic); stores it; rejects updates signed by revoked, retired or unknown keys or under expired trust; Update page shows the trust version, the revoked-key warning and the note about the switch being off | fake-feed tests: rotation picked up and the next update accepted; revocation picked up and an update signed by the revoked key refused; rollback and expired trust refused; no request at all while the switch is off and Check is not pressed |
| K4 | `br-sign` signs with a key held in a hardware security key or the Windows certificate store through CNG (`NCryptSignHash`, P-256), so release and root keys never exist as files | `br-sign` signs and verifies with a CNG key in a test; the file-based path still works |
| K5 | Ceremony checklists in `br-sign\README.md`; Windows Sandbox run of rotation and revocation end to end with throwaway keys (install signed with key A, publish rotation, update signed with B installs; then revoke A and check the warning) | sandbox run passes; first real `trust-1.json` generated by the maintainer before the first signed release |

K1 to K3 must land before the first signed release; K4 and K5 can follow, but K5's ceremony has to happen before the first real key is used.
