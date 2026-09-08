# Contributing to Oppen

Read [AGENTS.md](AGENTS.md), the relevant [specification](docs/spec.md), and
[license boundaries](LICENSES.md). A PR should identify the spec items or recorded
decision it implements and the behavior its tests prove. Dependencies require a
decision in `docs/decisions.md`. Contributions require the existing [CLA](CLA.md);
follow the CLA bot's exact signature prompt on your PR.

All changes to `main` go through PRs with current Rust formatting/Clippy/tests,
dependency checks, frontend checks and CLA checks passing. Resolve review
conversations before merge. CODEOWNERS routes review to the maintainer; it does
not establish an independent review for maintainer-authored PRs.

Outside contributors need maintainer approval before fork workflows run, even
after a previous contribution. Approval to run CI is not approval to merge.
Maintainers must inspect workflow and executable/build-script changes before
approving a run. Fork jobs must not receive release secrets. Never check out or
execute PR code in the privileged CLA workflow; its comments are data, not shell
commands. Only maintainers may request a CLA `recheck` comment rerun.

Release workflows, the signing public key, dependency locks, authority code,
licenses and this review policy require explicit maintainer attention. Actions
must use full commit SHAs and the repository's approved action list. Release key
changes require the migration procedure in the
[repository security runbook](docs/runbooks/repository-security.md).

Never commit credentials or real account data, including in old commits. Existing
upstream SDK test keys are fixtures, not operational credentials. Report suspected
leaks or vulnerabilities through [SECURITY.md](SECURITY.md), not a public issue.
