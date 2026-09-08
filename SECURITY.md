# Security reporting

Please report suspected vulnerabilities privately using
[Report a vulnerability](https://github.com/oppenxyz/oppen/security/advisories/new).
GitHub shares these reports with the repository's security maintainers rather
than publishing them as issues. You need a GitHub account.

Include the affected commit or app version, platform, reproduction steps,
expected and observed behavior, and a minimal proof of concept if available.
Use synthetic accounts and test data. Do not include private keys, seed phrases,
access tokens, real account exports or identifying information in public issues,
PRs, screenshots or logs. Coordinate any sensitive supporting material through
the private report. Testing does not authorize access to other people's accounts
or trading with their funds.

## Supported versions

Oppen is pre-alpha. Security fixes target the latest `main` and its current
development build; older development builds have no separate backport commitment.
A published build is not evidence that the [live acceptance gates](ROADMAP.md)
are complete. See the [threat model](docs/threat-model.md) for the boundaries of
the trading controls and the [update runbook](docs/runbooks/desktop-updates.md)
for signature verification and ledger downgrade restrictions.

Maintainers assess reports and coordinate fixes and disclosure through the
private advisory. There is no guaranteed response time or paid bounty program.
Ordinary non-sensitive bugs can use public issues.
