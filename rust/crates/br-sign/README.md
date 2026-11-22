# br-sign

Offline ECDSA P-256 release signing tool for BetterRecall (R8/R9, not shipped).

## Commands

- Generate a key pair (run once by the maintainer; keep the private key offline):
  ```
  br-sign keygen --private <private.pem> --public <public.pem>
  ```
  Refuses to overwrite existing files and prints the SHA-256 hex fingerprint of the public key's SPKI DER.

- Sign an install layout (`integrity.json` + `integrity.sig`):
  ```
  br-sign manifest <layout-dir> --key <private.pem> --version <version>
  ```

- Verify a signed install layout:
  ```
  br-sign verify <layout-dir> --public <public.pem>
  ```

- Pack a layout into a self-extracting installer exe:
  ```
  br-sign pack --stub <br-installer.exe> --layout <layout-dir> --version <version> --out <BetterRecall-Setup.exe>
  ```

- Sign a release update manifest (`update.json` + `update.sig`):
  ```
  br-sign update --installer <path-to-setup.exe> --version <version> --key <private.pem> --out <out-dir>
  ```

## Release builds

Set the public key path at build time:
- Rust: `BR_RELEASE_PUBKEY=<path-to-public.pem>`
- C#: `-p:BetterRecallReleasePubKey=<path-to-public.pem>`
