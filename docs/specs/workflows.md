# Trading workflows and agent profiles

**Component specification, v0.1 — draft**
Scope: v1.5 (deterministic orchestration) and v2 (in-app agent nodes)
Source material: `agentic_trading_terminal_coding_handoff.md`, adapted to oppen's
architecture. The handoff describes a hosted, multi-broker equities product; the
adaptation below is not cosmetic and the differences are stated in §2.

---

## 1. What this is

Two features that are one system:

- **Trading workflows** — node-graph orchestration with triggers, schedulers,
  conditions, loops and approval gates.
- **Agent profiles** — pre-built templates that model the functional structure of
  an investment desk, so the user deploys *a team*, not *a bot*.

The product abstraction is the workflow, not the agent. A workflow is a
persistent, resumable, auditable graph that coordinates specialized roles, each
with an explicit authority boundary.

```
   trigger ──▶ research ──▶ challenger ──▶ sizing ──▶ [risk gate] ──▶ [human gate] ──▶ execute ──▶ monitor
                    ▲                                      │                                        │
                    └──────────────── needs more research ──┘                        thesis broken ──┘
```

---

## 2. Adaptation from the source handoff

The handoff assumes a hosted web terminal, a workflow API, a broker layer and
equities with filings and earnings. oppen is a local-first desktop app on
Hyperliquid perpetuals with an external-agent trust model. Five differences are
load-bearing.

### 2.1 The Risk agent is not an agent

The handoff models Risk as an LLM that returns `APPROVE` /
`APPROVE_WITH_MODIFICATION` / `REJECT`, then states in Principle 6 that LLMs must
not enforce hard limits. Both cannot be true of the same component.

oppen already resolved this. The guardrail engine (`../spec.md` items 24–27) is a
deterministic predicate set evaluated in Rust immediately before signing, which
the model cannot reach. **The risk node in an oppen workflow is that engine, plus
`preflight`.** It is not a model call, has no prompt, cannot be persuaded, and
costs nothing per evaluation.

A workflow may additionally contain an advisory `risk_review` model node — for
judgment the predicates cannot encode, such as "this thesis is correlated with
the other three positions". That node can *lower* a size or *veto*. It can never
raise one, and it is never the last check.

### 2.2 Agents remain external in v1.5

D2 is settled: agents connect over MCP and hold no keys. The workflow engine does
not host models in v1.5. It orchestrates deterministic nodes and *waits for*
external agents, which pick work up through the existing event cursor.

This is the difference that makes the feature shippable early. See §4.

### 2.3 One container per workflow, not per node

D1 was revised on 2026-09-04: each agent gets one venue **account** — a
sub-account where the venue grants one, a top-level account where it does not
([../spec.md](../spec.md) D1, [../decisions.md](../decisions.md) V1). oppen calls
that account a container. A workflow has six roles and only one of them trades.

**A workflow instance is bound to exactly one container.** Its guardrails are
that container's guardrails. Every node inherits them. The execute node signs
with that container's agent wallet. Per-node attribution lives in the ledger, not
in venue state.

The conclusion survived the revision; the argument for it changed. "Binding a
sub-account per node would fragment margin" is still true — six containers are six
margin pools that do not net, because Hyperliquid nets per address per coin and
two containers are two addresses. What the revision adds is price. On Hyperliquid
a container is a top-level account the user derives in their own wallet, and
standing it up costs three wallet signatures — `approveAgent`,
`approveBuilderFee`, `usdSend` ([onboarding.md](onboarding.md) §3.3), or two with
the builder fee declined — plus one more withdrawal-capable key the operator
holds and backs up ([../decisions.md](../decisions.md), "The container's own
key"). A container per node charges that six times over for five roles that never
sign. On Aster and Lighter containers are cheaper — two signatures and one
API-key transaction respectively ([onboarding.md](onboarding.md) §4–§5) — and the
margin argument alone still settles it.

**Whose container it is, is not settled.** [../decisions.md](../decisions.md) R2
records the schema — `owner_type ENUM('agent','workflow')` plus `owner_id` — and
leaves the product rule open: a workflow may own a container of its own, or bind
to the container of the agent that drives it. One container per instance holds
either way. The branch decides whether two workflows can end up on one address,
which is §11's netting row and open decision 4 in §13.

### 2.4 Paper execution: cut

**Decided 2026-09-03: no paper broker. Testnet is the paper mode.** Testnet gives
real fills, real rejections, real latency and real funding, and exercises the real
signing path; a paper fill model is an assumption that would be optimistic in
exactly the ways the assumptions are wrong. The execute node has one
implementation, and §8 becomes "arm on testnet, promote to mainnet". See
[decisions.md](../decisions.md) S5.

The original argument, retained as superseded — §8 no longer carries this
dependency:

> The handoff is right that paper trading comes first, and oppen v1 has no paper
> mode. A workflow's execute node must be swappable between a paper broker and
> the live signer without the graph knowing which. This is added as a v1.5
> dependency (§8).

### 2.5 Instruments and evidence

There are no filings, earnings or analyst estimates on a perp venue. The
Researcher's evidence base is market structure: funding and carry, basis and
dislocation (`fair-value.md`), realized volatility, open interest, liquidity
depth, and whatever external sources the operator wires to their own agent. The
evidence-and-provenance requirement (handoff §20) survives intact and matters
more, not less: a claim about carry must cite the sample that produced it.

---

## 3. Templates ship structure, not alpha

The whitepaper states oppen ships no strategy, no signals and no default agent
behaviour. Templates do not violate this, and the boundary is worth writing down
because it will be tested by the first person who wants to ship a profitable
default.

A template MAY contain:

- the set of roles and the edges between them
- authority per role, and which gates are mandatory
- the shape of each role's structured output
- default guardrail values, chosen to be conservative rather than optimal
- prompts that describe a *responsibility* ("try to falsify this thesis")

A template MUST NOT contain:

- an entry or exit rule
- a parameter fitted to historical data
- a claim about expected return, win rate or Sharpe
- a default symbol list beyond what the operator allowlists

A template is an org chart with wiring. It has no opinion about what to buy.

---

## 4. Two layers, shipped separately

### 4.1 Layer one — deterministic orchestration (v1.5)

Everything in this layer runs in Rust with no model call. It is useful on its own
and works with today's external-agent architecture.

**Triggers.** A workflow run starts on one of:

| Trigger | Fires on |
|---|---|
| `schedule` | cron expression, or a fixed interval, in a named timezone |
| `price` | mark or fair value crossing a level, in absolute or σ units |
| `feature` | any `get_features` scalar crossing a threshold |
| `funding` | funding APR or `next_funding_s` crossing a threshold |
| `position` | liq distance, margin runway, unrealized PnL, time in position |
| `event` | any ledger event kind, including fills, refusals and guardrail trips |
| `manual` | operator presses run |

Triggers are evaluated in the core against the same snapshot the guardrails see.
A trigger that cannot evaluate — stale feed, missing reference — does not fire,
and records why. Fail-closed applies to starting work as well as to signing it.

**Scheduler.** Cron with second resolution, DST-correct via a named timezone
rather than a fixed offset, with catch-up policy per workflow: `skip` (default),
`run_once`, or `run_all` for missed windows. A machine that was asleep for six
hours must not wake up and fire six hourly runs into a market that has moved.

**Nodes available in layer one:**

`trigger` · `guardrail_check` · `preflight` · `place` · `cancel` · `cancel_all` ·
`close_position` · `wait` · `human_approval` · `condition` · `loop` · `fanout` ·
`journal_write` · `alert` · `halt`

**Waiting for an external agent.** The node `await_agent(role, schema, timeout)`
publishes a task to the ledger and blocks. A paired MCP agent sees it through
`get_events`, does the work, and answers with `submit_result(task_id, payload)`,
which is validated against the node's JSON schema before the run advances. The
agent never learns the workflow's internals beyond the task payload it is given.

This is the whole trick: an external Claude Code session becomes a node in a
graph it does not control, and the graph keeps its state, its gates and its audit
trail in the core.

### 4.2 Layer two — in-app agent nodes (v2)

Requires the BYO-model runtime already deferred to v1.5 in `../spec.md`. Adds
`agent(role)` nodes that call a model directly with the operator's own API key,
so a workflow runs unattended without an external client connected.

Layer two changes nothing about enforcement. An in-app agent node has exactly the
authority an external agent has: it proposes, and the guardrail engine disposes.

---

## 5. Roles

Six roles, matching the source handoff, with oppen's authority model applied.

| Role | Reads | Proposes | Modifies a proposal | Signs |
|---|---|---|---|---|
| `research` | market, features, journal | yes | no | no |
| `challenger` | the thesis, market, features | no | no | no |
| `sizing` | thesis, portfolio, guardrail headroom | yes | yes | no |
| `risk` (deterministic) | full order, portfolio, limits | no | reduces only | no |
| `execute` | approved order, book | no | no | **yes** |
| `monitor` | position, thesis, features | yes | no | no |

Three rules make this a control rather than a diagram:

1. **`risk` can only reduce.** Its output size is `min(proposed, permitted)`.
   There is no branch in which risk increases exposure.
2. **`execute` cannot originate.** It accepts an order object that only the risk
   node's success branch can construct — the same type-level constraint the
   signer already uses (`AGENTS.md` invariant 1).
3. **`challenger` cannot propose.** Its only outputs are a verdict and
   counterarguments. An adversarial role that can also propose collapses into a
   second researcher.

### 5.1 The challenger is worth the token cost

It is the one role in the handoff with no counterpart in a normal trading system,
and it is the one most likely to be cut for cost. Keep it. Its structured output
(`verdict`, `thesis_breakers`, `missing_information`) is the highest-signal
content in the ledger for post-hoc review: when a position loses money, the
question "was this failure mode already written down before we entered?" is
answerable, and answering it is how an operator learns whether their agent has an
edge or a style.

---

## 6. Workflow definition

Declarative, versioned, stored in SQLite, exportable as a file so a workflow can
be shared and diffed. Sharing a workflow file shares structure only; it never
contains keys, tokens, or the operator's container addresses.

```yaml
workflow: funding-carry-watch
version: 3
network: testnet
container: alpha-c1          # an existing container, by registry name
                             # `prompt` = arm behind the onboarding ceremony

guardrails:                  # the container's limits; nodes inherit
  symbols: [BTC, ETH]
  max_position_usd: 2000
  max_order_usd: 500
  max_slippage_bps: 15
  daily_loss_usd: 100
  order_rate: { count: 10, per_s: 60 }

triggers:
  - id: hourly
    type: schedule
    cron: "0 5 * * * *"
    tz: America/Sao_Paulo
    catch_up: skip
  - id: carry_spike
    type: feature
    symbol: BTC
    field: funding_apr_pct
    op: ">"
    value: 30

nodes:
  - id: research
    type: await_agent
    role: research
    timeout_s: 300
    schema: thesis.v1
  - id: challenge
    type: await_agent
    role: challenger
    timeout_s: 180
    schema: review.v1
  - id: size
    type: await_agent
    role: sizing
    schema: proposal.v1
  - id: check
    type: guardrail_check     # deterministic, always present
  - id: approve
    type: human_approval
    ttl_s: 900
  - id: fill
    type: place
    execution: { style: ioc, max_slippage_bps: 10 }
  - id: watch
    type: monitor
    every_s: 60

edges:
  - research -> challenge
  - challenge -> size        when: verdict in [PASS, PASS_WITH_CONDITIONS]
  - challenge -> research    when: verdict == NEEDS_MORE_RESEARCH
  - challenge -> end         when: verdict == REJECT
  - size -> check
  - check -> approve         when: check.ok
  - check -> end             when: not check.ok
  - approve -> fill          when: approved
  - fill -> watch
  - watch -> size            when: thesis_broken

limits:
  max_iterations: 3
  max_runtime_s: 3600
  max_model_cost_usd: 2.00
  max_orders_per_run: 4
```

`container` names a container that already exists in the registry. **oppen cannot
provision one on its own**, and the earlier `sub_account: auto  # provisioned on
first arm` was false as soon as D1 was revised: on Hyperliquid a container is a
top-level account the user derives in their own wallet, and it takes three
signatures in that wallet to stand up (§2.3). `container: prompt` is therefore
not auto-provisioning — it means arming blocks on the onboarding ceremony and the
workflow does not leave `draft` until the ceremony completes and the registry has
the address. On Aster and Lighter oppen can create the sub-account itself, so
`prompt` is cheaper there; it still is not silent, because the operator signs on
Aster and the key registration on Lighter needs the L1 key
([onboarding.md](onboarding.md) §4–§5).

Routing conditions are evaluated over structured fields only. There is no path
in which a natural-language string decides an edge.

---

## 7. Run state and the ledger

Workflow state is a projection of the existing hash-chained ledger (D6). No
second event store (`AGENTS.md` invariant 7).

Run states: `draft` `armed` `waiting_trigger` `running` `waiting_agent`
`waiting_approval` `executing` `monitoring` `paused` `completed` `failed`
`cancelled` `halted`.

New event kinds: `workflow.armed` `workflow.triggered` `workflow.node_started`
`workflow.node_completed` `workflow.node_failed` `workflow.routed`
`workflow.iteration` `workflow.limit_hit` `workflow.halted`.

The kill switch halts every workflow, cancels resting orders and refuses new
triggers. Halt state persists across restart (`../spec.md` item 26).

**Crash safety.** A run is resumable because every node transition is a ledger
append that commits before the node's side effect begins. On restart, a run in
`executing` reconciles by cloid before advancing — never by resending.

---

## 8. Arming: testnet, then mainnet

**There is no paper broker.** Testnet is the paper mode
([../decisions.md](../decisions.md) S5, §2.4 above). The execute node has one
implementation — the live signer — so there is no second fill model to maintain
and no branch in the graph that behaves differently in rehearsal.

Promotion is not a flag. A testnet container and a mainnet container are
different accounts on different networks, recorded in different database files
with different hash chains ([../decisions.md](../decisions.md) R4), so promoting
a workflow means binding its definition to a mainnet container that has been
through §2.3's ceremony. The arm dialog states the network in words and names the
container it will sign with; mainnet requires a second confirmation. P2 gates
unattended live execution on N clean testnet runs with no guardrail trips, and
editing the definition resets that counter because a fork is a new definition.

What this gives up, stated rather than smoothed over: a testnet book is thinner
than a mainnet book, so a fill that walked testnet depth is not evidence about
mainnet slippage. A paper broker would not have fixed that. It would have
modelled it, which is worse — the model would be an assumption maintained by us,
optimistic in exactly the ways the assumptions are wrong.

---

## 9. UI

A third tab, or an expansion of the existing Builder tab. Three views.

**Graph.** The node graph with live state per node, rendered in the ASCII system:
box-drawing edges, `+` for a completed node, `▓` for the running one, `·` for
not-yet-reached. Brightness marks state, as everywhere else.

**Run inspector.** Click a node to see its inputs, its structured output, its
evidence, latency, model cost, and the ledger sequence numbers it produced. The
causal chain from fill back to trigger must be one click per hop.

**Library.** Templates, versions, duplicate-and-edit. A workflow that has ever
traded live is immutable; editing forks it to a new version, so a run in the
ledger always points at a definition that still exists byte-for-byte.

---

## 10. Templates shipped at v1.5

Perp-native, structure only, all conservative by default.

| Template | Roles | What it structures |
|---|---|---|
| `funding-carry` | research → challenger → sizing → risk → human → execute → monitor | Carry with an explicit dead-zone check and a mandatory challenger pass |
| `basis-dislocation` | research → sizing → risk → execute → monitor | Trades `z_sigma` from `fair-value.md`; refuses when bar quality is not `Ok` |
| `vol-regime` | research → challenger → sizing → risk → human → execute → monitor | Sizes off `vol_ratio`, with a vol-scaled notional cap |
| `position-guardian` | monitor only | No entries. Watches liq distance and margin runway; proposes reductions |
| `research-only` | research → challenger | Mode A. No execute node exists in the graph at all |
| `custom` | — | Empty graph |

`position-guardian` and `research-only` are the two most useful and the two least
likely to be built first. `research-only` is the honest default for a new user:
it cannot trade because it has no execute node, not because a flag is off.

---

## 11. Failure modes this must handle

| Failure | Required behaviour |
|---|---|
| Agent never answers `await_agent` | Node times out, run enters `failed`, ledger records the timeout, no partial order |
| Agent answers twice | Second `submit_result` for a completed task is rejected as `stale_task` |
| Loop does not converge | `max_iterations` halts the run and records `limit_hit` |
| Trigger storms | Per-workflow trigger rate limit; coalesce identical triggers within a window |
| Two workflows, same symbol, different containers | Nothing nets. Hyperliquid nets per address per coin and two containers are two addresses, so both legs post full margin and each carries its own liquidation price. Exposure is summed across containers and never netted (D1); `FLEET_CAP` (v1.5 quant scope) bounds the aggregate |
| Two workflows, same symbol, one container | They **do** net, and every objection D1 raises to a shared address applies: no per-workflow margin, one liquidation price belonging to neither, and funding that cannot be split because at net zero no cash moved ([../decisions.md](../decisions.md) V3). D1's shared-address guards bind — a workflow may cancel or modify only order ids it opened, `close_position` and flatten are refused, equity and margin are shown once for the container and never mirrored per workflow. Whether oppen arms a second workflow onto an occupied container at all is **not decided** (§13.4) |
| Machine sleeps mid-run | On wake: reconcile, then either resume or fail the run. Never resume a run whose `place` node has an unreconciled cloid |
| Model cost runaway | `max_model_cost_usd` per run and per day; exceeded means halt, not truncate |
| Approval expires | Proposal expires with a typed event; the run does not silently proceed |

---

## 12. Acceptance gates

**Layer one (v1.5).** A scheduled trigger fires a `research-only` workflow on
testnet, an external Claude Code agent answers both `await_agent` nodes, the
guardrail node refuses an oversized proposal with a typed reason, and the full
causal chain from trigger to refusal is reconstructable from the ledger with no
gaps. No order is placed, because the template has no execute node.

**Execution (v1.5).** The same workflow with an execute node, armed on testnet
against its own container, fills on the live testnet book, and its realized PnL
matches the venue's `closedPnl` for those fills ([history.md](history.md) §5) —
the venue's number, not a local recomputation used as its own proof.

**Layer two (v2).** The same workflow runs unattended with in-app agent nodes,
overnight, and every order it placed passes the same guardrail path as an
externally-driven one — proven by the type system, not by inspection.

---

## 13. Open decisions

1. **Where workflows live in the UI.** New tab, or the existing Builder tab
   promoted? The design mock already reserves Builder for exactly this.
2. **Schema versioning for `submit_result`.** Named schemas (`thesis.v1`) imply a
   registry the agent can fetch. That registry is a new MCP surface.
3. **Whether `sizing` and `research` collapse** for a first release. They are
   distinct in a fund; in a one-person deployment the same model answers both.
   Recommend keeping them separate in the graph and allowing the same agent
   identity to serve both roles.
4. **Cross-workflow netting, and whether two workflows may share a container.**
   Two questions, split by §11's two rows. *Across* containers there is nothing
   to net and the answer is `FLEET_CAP`: portfolio-level guardrails spanning
   containers, which the whitepaper already lists as post-v1. *Within* one
   container the positions net at the venue, so the question is not aggregation
   but whether oppen arms the second workflow at all. That branch is downstream
   of R2 landing on `agent` or `workflow` ([../decisions.md](../decisions.md)),
   which is itself open; nothing in v1.5 should assume either answer.
