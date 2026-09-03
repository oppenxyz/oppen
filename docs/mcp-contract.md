# oppen MCP contract

Version: `0` (pre-release, unstable). The contract is versioned from the first public release; the envelope carries `contract_version`.

This document is the normative reference for every tool the gateway exposes. It is written when phase P4 lands; until then, `docs/spec.md` section C is the specification.

## Principles

- Deterministic JSON: stable key order, units in field names, pre-rounded values, timestamps no finer than the field needs.
- Every failure is typed. Retryability is part of the type.
- `reason` is required on every action and is treated as untrusted text.
- One cursor: `get_events(since)` over the ledger rowid. Cursor too old → `resync_required`.

## Tools (v1)

`get_state` · `get_meta` · `get_events` · `get_features` · `preflight` · `place` · `cancel` · `cancel_all` · `close_position` · `get_order_status` · `remember` · `recall` · `set_alert`

Schemas, examples and the error taxonomy follow in P4.
