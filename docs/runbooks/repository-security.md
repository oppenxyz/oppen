# Public repository safeguards

Scope: `oppenxyz/oppen` only. `oppenxyz/oppen-website` stays private. Decisions
PUB1–PUB3 implement spec item 1 and the owner's public-repository hardening request.

## Main and Actions policy

- `main` requires a PR, resolved review conversations, and an up-to-date branch
  with `rust · fmt · clippy · test`, `cargo deny`, `web · typecheck · build` and
  `cla` passing. Required checks are bound to GitHub Actions (app ID 15368).
- These rules include administrators. Force pushes and branch deletion are off.
- There is one maintainer (`gkssxf`). Required approval count is zero to avoid
  making that maintainer's own PRs impossible to merge. CODEOWNERS requests owner
  review; it is not a second-person security boundary. When a second trusted
  maintainer is appointed, enable at least one required approval and required
  code-owner review. Do not silently bypass checks for automated merges.
- All external fork contributors require workflow approval. Default tokens are
  read-only and workflows cannot approve PR reviews. No self-hosted runners.
- Full action SHA pinning is required. Only `actions/checkout`,
  `dtolnay/rust-toolchain`, `Swatinem/rust-cache`, `oven-sh/setup-bun`,
  `EmbarkStudios/cargo-deny-action` and `contributor-assistant/github-action` are
  allowed. A new action requires review and an explicit allowlist update.
- The pinned CLA action can write signatures to `cla-signatures`, comment/lock
  PRs and rerun its own failed workflow. Its unused `statuses: write` permission
  is removed. Its job handles PR lifecycle events, explicit signatures, and
  trusted maintainer `recheck` comments only. It never checks out PR code.
  Keep the signature-storage branch writable by this action; it cannot satisfy
  `main`'s protected PR/check requirements by writing a signature file.
- Secret scanning, push protection, Dependabot alerts and security-update PRs
  are enabled. Review updates through the same gates; no automatic merging.
- Private vulnerability reporting is enabled; [SECURITY.md](../../SECURITY.md)
  links directly to the private advisory form. Treat scanners as detection aids,
  not proof that code or history is secret-free.

## Release key migration

The `desktop-release` environment allows only the branch `main` (not a tag named
main). Release jobs declare this environment and still depend on the full CI
suite. No per-release manual approval is added: reviewed, passing main commits
continue to publish automatically.

1. Create the environment with a custom branch policy for `main`, type `branch`.
2. Store the existing `TAURI_SIGNING_PRIVATE_KEY` as an encrypted environment
   secret, from its private local backup. Never print it, put it in an argument,
   commit it, or regenerate it. Upload requires explicit credential authorization.
3. Merge the workflow's environment declaration after CI passes. Confirm the
   environment has the key; an environment secret overrides a same-name repository
   secret during this transition.
4. Delete the repository-wide copy, then verify a main release can build, sign and
   publish all three assets using the environment secret alone. Do not leave the
   broader copy as a permanent fallback. An environment or signing failure blocks
   publication; diagnose it without widening access or disabling signatures.

The unchanged public key keeps existing clients compatible. This restriction
prevents ordinary feature-branch jobs from obtaining the signing key, but cannot
protect against a malicious change already merged to main or a compromised
repository administrator. Keep account MFA and credential recovery under owner
control. The local backup stays outside the repository with owner-only access.

## Verification and maintenance

Read back branch protection, Actions permissions, fork approvals, environment
branch policies and secret *names*, and private-reporting status with `gh api`.
Do not print secret values. Verify CODEOWNERS parses without errors, PR CI passes,
and a signed main release succeeds after the repository key is removed. Confirm
the website's visibility remains private. A release signature is not Apple
notarization or a completed trading acceptance gate.

Counsel review of the custom desktop license and CLA remains a separate owner
task; the [license page](../../LICENSES.md) records the questions without changing
any rights. Repository publication cannot recall copies others already obtained.
