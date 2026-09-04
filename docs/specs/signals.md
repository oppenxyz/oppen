# Opt-in signal publishing and the community layer

**Component specification, v0.1 — draft**
Scope: v2. Nothing here ships before the guardrail, approval and ledger phases are
mature.

---

## 1. What this is

Two directions, both opt-in, both off by default:

- **Publish** — an operator chooses to emit their signals, and optionally their
  portfolio, from oppen to the oppen website, where others can read them.
- **Subscribe** — an operator follows another publisher and sees their signals in
  their own console.

The product framing is a community of operators, not a signal marketplace. The
distinction is not marketing: a marketplace implies the signals are worth money,
which implies a claim about their performance, which is a claim oppen is not in a
position to make about a stranger's agent.

---

## 2. The architecture problem, stated plainly

The whitepaper says, in public, that oppen has no backend, no account, and no
telemetry path carrying positions. This feature requires a server. That
contradiction has to be resolved in the design, not in the copy.

### 2.1 The resolution

**The server is not part of oppen.** It is a separate product — the website —
with its own repository, its own threat model, and no privileged relationship to
the app. The app's relationship to it is the same as its relationship to any
other website: it can be handed data, and it can be read from.

Four rules make that true rather than merely stated.

1. **Publishing is a push, always operator-initiated, never ambient.** No
   background sync, no "opt out later" default, no telemetry channel that happens
   to also carry signals. The app posts when the operator publishes.

2. **The app takes no instruction from the network.** There is no command channel,
   no remote config, no server-supplied allowlist, no kill switch operated by
   anyone but the operator. Subscribed content is inert data rendered as text.

3. **No subscribed content can reach the signing path.** This is the hard one and
   it is stated as an invariant in §4.

4. **Keys, tokens and addresses never leave.** A published signal contains no
   address by default. Identity is a publisher pseudonym the operator chooses,
   not a wallet.

The whitepaper's claim is then still true with one sentence added: no component
runs anywhere but the operator's machine, and the operator may separately choose
to publish to a service that does. That sentence goes in the whitepaper when this
ships. Shipping the feature without amending the claim would be the actual
problem.

---

## 3. What a signal is

A signal is a **statement about intent, already acted upon**, not a
recommendation. The distinction runs through the whole design.

```json
{
  "publisher": "pseudonym",
  "published_at": 1788460183896,
  "symbol": "BTC",
  "direction": "long",
  "kind": "open",
  "size_pct_of_book": 12.4,
  "entry_band_bp": 8,
  "reason": "…operator-authored or agent-authored text…",
  "reason_author": "agent",
  "thesis_ref": "opaque id, resolvable only if the publisher also published it",
  "delay_s": 300,
  "network": "mainnet"
}
```

Deliberate omissions and why:

- **No absolute size.** Percentage of the publisher's own book only. Absolute
  notional tells a reader how much capital is behind a position, which is both a
  privacy leak and the number that makes copying feel safe when it is not.
- **No exact entry price.** A band in basis points. An exact fill price is a
  fingerprint that can be matched against the public chain to deanonymize the
  publisher.
- **No stop or target by default.** Publishing where your stop sits is publishing
  where to hunt you.
- **A publication delay, default non-zero.** Immediate publication of a live
  intent invites front-running of the publisher. The default is a delay the
  operator sets once, and the UI explains what it protects against.

`reason` is agent-authored text in most cases and is therefore untrusted on the
way out too: it is the publisher's claim, labelled as such, and it is sanitized
before it leaves as well as before it renders.

---

## 4. The invariant

> **No subscribed content may reach the signing path.**

A subscribed signal is a rendered row. It is not an order, it is not a
pre-filled ticket, and it is not an input to any agent unless the operator
deliberately wires it there.

This is the same class of boundary as `AGENTS.md` invariant 9 (agent `reason`
strings are inert), and it exists for the same reason, one step more serious. An
agent's reason string is written by software the operator chose to run. A
subscribed signal is written by **a stranger**, and it arrives through a channel
the operator did not authenticate. If a subscribed field could set a symbol, a
size or a price, the community layer would be a remote-influence channel into
other people's trading, dressed as a feature.

Three consequences:

- **No auto-copy. Ever.** Not behind a flag, not with a confirmation, not "for
  advanced users". A copy-trading path with guardrails is still a path in which a
  stranger's text moves an operator's money, and the guardrails only bound the
  damage, they do not authorize it.
- **Acting on a signal is a manual, retyped action.** The operator reads the row
  and places their own order in their own ticket. There is no button that carries
  a value from the row into the ticket.
- **If an agent is to consume signals**, it happens because the operator wired
  their own agent to their own subscription feed outside oppen, and the resulting
  orders pass the same guardrails as any other. oppen does not provide that wire.

---

## 5. Portfolio sharing

Separately opt-in, separately toggled, and coarser than signals.

Published: percentage allocation by symbol, direction, aggregate leverage bucket,
and a performance series **normalized to a starting index of 100**. Not published:
absolute equity, absolute size, addresses, sub-accounts, or open orders.

Performance display rules, which are the whole risk here:

- Every statistic carries `n` and a standard error. This is already the rule for
  the operator's own analytics ([history.md](history.md) §4) and it applies with
  more force when a stranger reads it.
- Time-weighted return only, over a stated window, with the window always visible.
- Drawdown shown alongside return, never return alone.
- Testnet and mainnet are separate and cannot be aggregated. A testnet track
  record shown next to a mainnet one, without labels, is a lie by layout.
- No leaderboard ranked by return. `../spec.md` section F already rejects
  raw-PnL leaderboards fed to agents; feeding them to humans produces the same
  behaviour, which is size-chasing the top of a table that is mostly variance.
  Sort by anything else: recency, symbol, activity, drawdown.

---

## 6. Privacy and consent

- **Opt-in per capability**, not once globally: publishing signals, publishing
  portfolio, and being listed publicly are three switches.
- **A preview before the first publish** that shows the exact JSON leaving the
  machine, field by field. Not a summary. The operator sees the payload.
- **Revocation deletes.** Unpublishing removes the content from the service.
  Whether it removes it from caches, mirrors and anyone who already read it is a
  question the UI must answer honestly: it cannot.
- **A published signal is permanent in practice.** Say so at the first publish,
  once, plainly, and do not repeat it into meaninglessness.
- **Pseudonyms are not anonymity.** A publisher who posts every fill on a public
  chain with a consistent delay is identifiable by correlation. The onboarding
  says this.

---

## 7. Regulatory note

This is not legal advice and the spec is not the place to give it, but the design
has to account for the question.

Publishing trading signals to subscribers, particularly for compensation, is
regulated activity in many jurisdictions, and the definitions of investment
advice are broad. Three design consequences, independent of how the legal
question resolves:

1. **No payment rail in v2.** Free publication only. Introducing compensation
   changes the analysis in most jurisdictions and should not be a side effect of
   a community feature.
2. **No performance claims by oppen.** The service displays what publishers
   published, with `n` and error bars, and makes no claim about any publisher.
3. **Copy is manual by construction** (§4), which is a safety property first and
   a jurisdictional one second.

Before this ships, the question gets a real answer from someone qualified, in the
jurisdictions where the entity operates.

---

## 8. Trust and abuse

The failure modes are social, and they are the reason this is a v2 feature rather
than a v1.1 one.

| Failure | Mitigation |
|---|---|
| Publisher pumps a thin market, then publishes the exit | Publication delay; size as % of own book; no absolute size to chase |
| Fabricated track record | Publish from ledger-derived data only, never operator-typed; note that oppen cannot prove a stranger ran unmodified code |
| Survivorship: publisher deletes losing signals | Publications are append-only on the service. Deletion marks deleted, does not erase the record of having published |
| Coordinated crowding | The whitepaper's own counterargument, §11. Fleet crowding metrics exist for the operator's own agents; they do not extend across strangers |
| Injection through `reason` | Sanitized on send and on render; rendered as plain text; never parsed by anything |

**oppen cannot verify that a publisher is running unmodified oppen.** The license
permits forks; a fork can publish anything. The service therefore attests to
nothing about the source of a signal, and says so where the signals are displayed.

---

## 9. Acceptance gates

1. **Isolation.** With the network unavailable, every part of oppen except the
   publish button behaves identically. Signals are not on any startup path.
2. **No path to the signer.** A test that attempts to construct an order from a
   subscribed signal fails to compile. The type carrying subscribed content has
   no conversion into an order.
3. **Payload honesty.** The preview shown before publishing is byte-identical to
   what the network sees, verified by capture.
4. **Revocation.** Unpublish removes the content from the service within a stated
   window, and the UI states what unpublishing cannot undo.

---

## 10. Open decisions

1. **Whether this belongs in this repository at all.** The strongest version of
   the architecture puts publishing in a small separate crate with no dependency
   on the signer, and the service in the website repository. Recommend that.
2. **Identity.** Pseudonym plus a locally-held keypair for signing publications
   gives continuity without an account. It also creates a second key to manage.
3. **Transport.** A signed JSON POST to a documented endpoint is enough. Anything
   more interactive re-opens the command-channel question closed in §2.1.
4. **Whether portfolio sharing ships at all**, or only signals. Portfolio sharing
   carries most of the privacy risk and most of the performance-claim risk for a
   smaller share of the value.
