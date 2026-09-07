# Private desktop update channel

Owner-approved decisions UP1–UP4. First target: Apple Silicon macOS, private
`oppenxyz/oppen` releases, personal builds without Apple notarization.

## Everyday use

Merge to main. The existing `ci` workflow must pass Rust formatting, Clippy,
workspace tests, dependency checks and frontend checks before its release job
runs. That job builds `aarch64-apple-darwin`, assigns version `0.1.<CI run number>`,
signs the updater archive and publishes `desktop-v<version>` in the same private
repository. Version numbers can have gaps because pull requests also run CI.

The installed app checks at launch and hourly while open. It reads the current
GitHub CLI login using `/opt/homebrew/bin/gh` (or `/usr/local/bin/gh`) and needs
read access to this repository. No repository token is embedded in the app,
returned over IPC or written into release metadata. CLI diagnostics and updater
errors are mapped to static UI messages. The downloaded archive's Tauri signature
must verify against the public key bundled in the installed app.

Click **Update ready** in the footer, then **Restart to update**. Confirm with
**Install and restart** when the session may be interrupted. This does not cancel
orders or close positions. Background checking/downloading never restarts the app.

## Signing and bootstrap

`TAURI_SIGNING_PRIVATE_KEY` is an encrypted Actions repository secret. A private
local backup is kept outside the repository in `~/.oppen/release-signing/`, with
owner-only directory/file permissions. Keep that backup secure: losing the key
prevents delivering updates to already installed clients. The public key lives
in `apps/desktop/src-tauri/tauri.conf.json`; do not casually regenerate it.

Existing installations need one updater-enabled replacement of
`/Applications/oppen.app`. Preserve identifier `xyz.oppen.desktop`, close that
app before replacing it, and retain the old bundle as a bootstrap backup. Only
the application bundle is replaced. Keychain entries, gateway data directories
and WebKit preferences are not copied, deleted or migrated by the installer.

macOS code signing is ad-hoc (`-`). Tauri's update signature is mandatory but is
not an Apple Developer ID signature. General public distribution requires a
Developer ID certificate and notarization; neither is configured for this
personal channel. The initial downloaded bundle may need explicit macOS Open
approval. Never remove quarantine recursively from unrelated files.

## Failure and recovery

- Failed CI produces no release. Draft releases are invisible to the app until
  archive, signature and manifest uploads have completed.
- Release jobs are serialized. Published versions never decrease, even if older
  CI runs finish later. Rerunning an already published version is a no-op;
  rerunning an incomplete draft can finish its uploads.
- An authentication failure: run `gh auth status` / `gh auth login` on the Mac,
  then use **Check for updates**. No credentials need to be pasted into Oppen.
- Download or signature failure installs nothing. Retry checks; a signature
  failure must never be bypassed.
- Update bytes are held only in Rust memory until explicit installation. Closing
  the app before installing discards them; the next check downloads again.
- A failed installation is reported. Preserve the running session if possible
  and reinstall a verified release bundle if necessary. Tauri handles bundle
  replacement; this is not a database rollback mechanism.
- Roll forward with a corrective main commit. Do not downgrade across ledger
  reader barriers (currently V5). Restoring an older app does not downgrade data.

## Validation

`cargo test -p oppen-desktop` covers repository asset URL restrictions and
manifest selection. `python3 -m unittest discover -s scripts -p 'test_*.py'`
covers release metadata/version rules. Run the normal frontend build/tests and
workspace Clippy before promotion. End-to-end acceptance additionally requires
a signed GitHub release, a native download that reports verified readiness,
explicit installation/restart and the new installed version on the next launch.
