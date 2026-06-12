# Release download verification

Every artifact a user downloads — the CLI archive, the desktop package, and any
over-the-air firmware image — should be verifiable before it runs. This file
records what is already wired and the **cross-repo steps that must accompany a
release** (the public `nateschnell/agent-buddy` repo owns its own
`.github/workflows/release.yml`, so those parts can't live in the monorepo).

There are two layers:

- **Integrity** (SHA-256): catches a corrupt/partial/truncated download. Wired
  end-to-end today.
- **Authenticity** (signatures): catches a *swapped* asset (e.g. a release-asset
  or account compromise). A documented follow-on — see the last section.

---

## 1. Already implemented (in this monorepo)

### Installers verify the CLI archive
`install.sh` and `install.ps1` download `SHA256SUMS` from the same release,
verify the archive's hash, and **fail closed** (refuse to install) on a missing
manifest, a missing entry, or a mismatch. They also preflight `curl`/`tar`,
enable `pipefail`/TLS 1.2, and confirm the extracted binary exists.

### Daemon verifies an OTA firmware download
`update::download_firmware_verified` fetches the image's sibling
`firmware-<board>.bin.sha256` checksum and verifies the bytes **before** the
device is ever put into flash mode. (The device's own MD5 only covers the BLE
transfer — the daemon hashes whatever it downloaded, so a corrupt download would
otherwise be flashed.) Releases that predate checksum publishing fall back to the
prior behavior with a logged warning.

### This repo's `release.yml` publishes firmware checksums
The `firmware` job emits a per-image `firmware-<board>.bin.sha256` sidecar (bare
hex digest) and a combined `SHA256SUMS`, and attaches them to the release.

---

## 2. Required companion changes in the PUBLIC repo's `release.yml`

These live in `nateschnell/agent-buddy/.github/workflows/release.yml` and must
ship together with a release for the verification above to succeed.

1. **Publish `SHA256SUMS` for the CLI archives + desktop packages.** After the
   `agent-buddy-<target>.tar.gz` / `.zip` archives are built, generate a
   `SHA256SUMS` over them and attach it to the public release so the installers
   can verify:
   ```yaml
   - name: Checksums
     run: (cd dist && sha256sum agent-buddy-*.* *.dmg *.AppImage *Setup*.exe 2>/dev/null > SHA256SUMS || true)
   # …then add dist/SHA256SUMS to the release `files:` list.
   ```
   Until this ships, `install.sh` / `install.ps1` will (correctly) refuse to
   install, since they fail closed on a missing manifest.

2. **Carry the firmware `.sha256` sidecars into the public release.** The job
   that fetches `firmware-<board>.bin` from this repo's private release must also
   fetch the matching `firmware-<board>.bin.sha256` files, re-attach them to the
   public release, and stage them next to the binaries for bundling (so the
   bundled/offline path has them too).

3. **Stage Windows installer assets.** `make-app.sh` and `make-appimage.sh`
   already assemble the bundled notices from `bridge/assets/LICENSE` +
   `bridge/assets/IBMPlexSans-LICENSE.txt`; the Inno Setup build
   (`installer.iss`, `[Files]`) expects a `THIRD_PARTY_LICENSES` in its
   `StageDir`. Concatenate the same two files into `StageDir/THIRD_PARTY_LICENSES`
   before `iscc`. Also copy `bridge/assets/app-icon.ico` to
   `StageDir/app-icon.ico` so the installer, Start Menu shortcut, and optional
   desktop shortcut use the Agent Buddy icon.

---

## 3. Follow-on: authenticity via a signed manifest

SHA-256 alone proves integrity, not authenticity — an attacker who can replace a
release asset can replace its `SHA256SUMS`/sidecar too. To defend the firmware
OTA and the installers against a swapped/forged asset, sign the manifest with a
key that does **not** live in the release:

1. **Generate a signing keypair** (e.g. `minisign -G`, or Ed25519). Keep the
   **secret** key in a CI secret (`MINISIGN_SECRET_KEY`); commit the **public**
   key into the source (a `const` in `update.rs` and a literal in the installers).
2. **Sign in CI**: after generating `SHA256SUMS`, produce `SHA256SUMS.minisig`
   and attach it to the release (both this repo's firmware job and the public
   repo's archive job).
3. **Verify before trusting**:
   - Installers: fetch `SHA256SUMS` + `.minisig`, verify the signature against the
     embedded public key, *then* check the archive hash.
   - Daemon: `download_firmware_verified` gains a signature check — fetch the
     manifest + signature, verify against the baked-in public key, then compare the
     image hash. Flip the `None`-checksum fallback from "warn and proceed" to
     "refuse" once signing is live.

This is a drop-in extension of the existing hash plumbing (the data already flows
through `FirmwareLatest.sha256_url` and the installers' `SHA256SUMS` fetch); it
adds the key, the `.minisig` asset, and one verify step on each side.
