# oppen feature specifications

One document per feature area, written before implementation. Each is a draft
until it carries a sign-off line. `../spec.md` remains the normative v1
specification, and the architecture decisions D1–D8 in it are settled; nothing
here re-opens one. D1 has been revised exactly once, on 2026-09-04, in the way
described below.

| Spec | Feature | Target | Status |
|---|---|---|---|
| [venue-containers.md](venue-containers.md) | The container model, and what Hyperliquid, Aster and Lighter each grant | v1 · P2 (model) / v2 (Aster, Lighter) | draft |
| [onboarding.md](onboarding.md) | First-run ceremony: container per agent, agent wallet, builder fee, pairing | v1 · P7 | draft |
| [workflows.md](workflows.md) | Trading workflows and agent profiles | v1.5 / v2 | draft |
| [history.md](history.md) | Portfolio tab: durable trading history | v1.1 | draft |
| [charts.md](charts.md) | ASCII candles, arbitrary intervals, line chart, Quantoppen | v1 / v1.1 | draft |
| [fair-value.md](fair-value.md) | Fair value engine (feeds Quantoppen) | v1.5 | draft |
| [signals.md](signals.md) | Opt-in signal publishing and the community layer | v2 | draft |
| [mobile.md](mobile.md) | Mobile companion | v2 | draft |

## How these relate to the settled architecture

Four things a reader needs before opening any of the specs above. The first is a
change to the settled set itself. The other three are tensions between a feature
and a claim the whitepaper makes in public; each of those specs opens with its
tension and how it resolves.

**D1 changed.** It read "One Hyperliquid sub-account per agent" and now reads
"one venue *account* per agent". The old wording named a primitive a new account
cannot obtain: Hyperliquid gates sub-accounts behind $100,000 of traded volume,
protocol-enforced rather than a UI guard, at the same threshold on testnet, and
the refusal was observed from the project's own wallet on 2026-09-04 —
`Required: $100000. Traded: $0`. The unit of isolation is now the **container**:
a sub-account where the venue grants one, a top-level account where it does not,
with the $100k path kept as an upgrade rather than a requirement. Two
consequences reach the specs here — a container is addressed by its own address
rather than as a label under a master, and Aster and Lighter become first-class
in the architecture rather than exceptions to it, since both grant sub-accounts
with no volume gate. First-class is not a shipping commitment; v1 is still
Hyperliquid only. Model: [venue-containers.md](venue-containers.md). Ceremony:
[onboarding.md](onboarding.md). Reasoning: [../decisions.md](../decisions.md)
V1–V6. What it did *not* change: D5 holds completely, because every signature the
new model adds is signed in the user's own wallet and no key path appears in the
app.

The mechanism matters as much as the outcome. A settled decision changes by being
rewritten in `../spec.md` with its reasoning recorded in `../decisions.md`, and
every document the old wording made false named there. It does not change by a
pull request arguing against it, and a spec in this directory still cannot
re-open one.

**"No backend."** The signal layer is the only feature that involves a server.
It resolves by keeping the server outside the app: publishing is an explicit
operator push, the app never takes instruction from the network, and no
subscribed content can reach the signing path. See [signals.md](signals.md) §2.

**"Ships no strategy."** Workflow templates ship *structure* — roles, gates,
routing, limits — not alpha. No template contains an entry rule, a parameter
fitted to history, or a claim about expected return. See
[workflows.md](workflows.md) §3.

**"Local-first desktop."** The mobile companion is read-only by construction and
cannot host agents. Its one write capability is cancellation. See
[mobile.md](mobile.md) §3.

## Conventions

- Every spec states its acceptance gate. A gate is a falsifiable test, not a
  feature list, matching the P0–P7 convention in `../../ROADMAP.md`.
- Numbers carry units in the identifier (`slip_bps`, `carry_usd_per_day`).
- Anything an agent can read is untrusted on the way in and inert on the way
  out. `AGENTS.md` invariant 9 applies to every new surface.
- A venue fact carries the page it was read from and the date it was read. A
  fact that could not be confirmed is written down as unconfirmed in the spec
  that depends on it, never omitted.
