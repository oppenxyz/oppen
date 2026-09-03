# oppen feature specifications

One document per feature area, written before implementation. Each is a draft
until it carries a sign-off line. `../spec.md` remains the normative v1
specification and the architecture decisions D1–D8 in it are settled; nothing
here re-opens them.

| Spec | Feature | Target | Status |
|---|---|---|---|
| [workflows.md](workflows.md) | Trading workflows and agent profiles | v1.5 / v2 | draft |
| [history.md](history.md) | Portfolio tab: durable trading history | v1.1 | draft |
| [charts.md](charts.md) | ASCII candles, arbitrary intervals, line chart, Quantoppen | v1 / v1.1 | draft |
| [fair-value.md](fair-value.md) | Fair value engine (feeds Quantoppen) | v1.5 | draft |
| [signals.md](signals.md) | Opt-in signal publishing and the community layer | v2 | draft |
| [mobile.md](mobile.md) | Mobile companion | v2 | draft |

## How these relate to the settled architecture

Three of these features touch claims the whitepaper makes in public. Each spec
opens with the tension it creates and how it resolves. In summary:

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
