# macOS code signing + notarization

How the `Agent Buddy.app` / `.dmg` is signed with a **Developer ID
Application** certificate and notarized by Apple so users can open it with no
Gatekeeper prompt (and so the desktop app can eventually self-update — see
`bridge/src/bin/app.rs` `update_card`).

`packaging/macos/make-app.sh` already does the signing: pass a real identity in
`MACOS_SIGN_IDENTITY` and it signs the nested daemon then the bundle with the
**hardened runtime** (`--options runtime`) and a **secure timestamp** — both
required by notarization. Absent that env var it ad-hoc signs (unsigned, runs
with a one-time right-click→Open). Notarization + stapling is a separate step,
done on the finished `.dmg` (locally below, or in CI via `.github/workflows/release.yml`).

## One-time prerequisites (Apple Developer account)

1. **Developer ID Application certificate.** Xcode → Settings → Accounts →
   Manage Certificates → ＋ → *Developer ID Application*. Confirm with:
   ```bash
   security find-identity -v -p codesigning
   # look for: "Developer ID Application: NAME (TEAMID)"
   ```
   That full string is your `MACOS_SIGN_IDENTITY` (the `TEAMID` in parentheses
   is your Apple Developer Team ID).

2. **App Store Connect API key** (for `notarytool`). App Store Connect → Users
   and Access → Integrations → App Store Connect API → generate a key with the
   **Developer** role. Save the three values:
   - Issuer ID (UUID)
   - Key ID (also encoded in the filename `AuthKey_<KEYID>.p8`)
   - the `.p8` private key file — **downloadable only once**

## Build + sign + notarize locally

```bash
# 1. Build the binaries (universal optional locally; arm64-only is fine to test)
cd bridge && cargo build --release --features gui && cd ..

# 2. Build + sign the .app/.dmg
MACOS_SIGN_IDENTITY="Developer ID Application: NAME (TEAMID)" \
  bash packaging/macos/make-app.sh \
    --bin-dir bridge/target/release \
    --out out \
    --version "$(git describe --tags --always --dirty)"

# 3. Sanity-check the signature (before notarizing)
codesign -dvvv "out/Agent Buddy.app"      # expect flags=…(runtime), Timestamp=…,
                                            # Authority chain ending in Apple Root CA
codesign --verify --strict --verbose=2 "out/Agent Buddy.app"

# 4. Notarize the .dmg + staple the ticket
DMG="$(ls out/*.dmg)"
xcrun notarytool submit "$DMG" \
  --key   /path/to/AuthKey_XXXX.p8 \
  --key-id XXXX \
  --issuer 00000000-0000-0000-0000-000000000000 \
  --wait
xcrun stapler staple "$DMG"

# 5. Prove Gatekeeper accepts it
xcrun stapler validate "$DMG"
spctl -a -t open --context context:primary-signature -vv "$DMG"   # -> "accepted, source=Notarized Developer ID"
```

If notarization is rejected, pull the detailed log:
```bash
xcrun notarytool log <submission-id> --key … --key-id … --issuer …
```
First-ever submissions from a new account commonly sit `In Progress` 15–30 min;
later ones are usually quick.

## CI (`.github/workflows/release.yml`, job `package-macos`)

The job signs + notarizes automatically when the secrets below are set,
otherwise it falls back to an ad-hoc `.dmg` (the job still succeeds, with a
`::warning::`). Set these as **repository (or environment) secrets**:

| Secret | Value |
| --- | --- |
| `MACOS_CERTIFICATE_P12` | `base64 -i cert.p12` (the exported cert **+ private key**) |
| `MACOS_CERTIFICATE_PWD` | the password you set when exporting the `.p12` |
| `MACOS_SIGN_IDENTITY`   | `Developer ID Application: NAME (TEAMID)` |
| `MACOS_NOTARY_KEY_P8`   | `base64 -i AuthKey_XXXX.p8` |
| `MACOS_NOTARY_KEY_ID`   | the Key ID (the `XXXX` in `AuthKey_XXXX.p8`) |
| `MACOS_NOTARY_ISSUER`   | the Issuer ID (UUID) |

Encode the two files like so (pbcopy puts the base64 on your clipboard):
```bash
base64 -i cert.p12             | pbcopy   # -> MACOS_CERTIFICATE_P12
base64 -i AuthKey_XXXX.p8      | pbcopy   # -> MACOS_NOTARY_KEY_P8
```

## Why this is safe for a PUBLIC repo

- **No fork can read the secrets.** `release.yml` triggers only on `push` of
  `v*` tags and `workflow_dispatch` — there is **no `pull_request` trigger**, so
  pull requests (including from forks) never run this workflow and never receive
  these secrets. GitHub also never exposes secrets to workflows triggered by
  fork PRs in the first place. Do **not** add a `pull_request_target` trigger to
  this workflow.
- **Only the public half of signing is ever shipped.** The Team ID and the
  certificate's public identity are embedded in every signed binary anyway; the
  *private* key never leaves the `.p12` secret.
- **Ephemeral, shredded credentials.** The cert is imported into a throwaway
  keychain in `$RUNNER_TEMP` (gone when the runner is torn down); the decoded
  `.p8` is `rm`-ed immediately after `notarytool` runs. Nothing is written to
  the repo or logged.
- **Least privilege.** The notary API key uses the **Developer** role, not
  Admin, and is independently revocable in App Store Connect.

Hardening options if you want extra isolation: put the six secrets in a GitHub
**Environment** with required-reviewer protection so they only unlock on
release runs, and/or pin the runner. Neither is required for correctness.
