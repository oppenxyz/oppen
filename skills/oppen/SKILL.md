---
name: oppen
description: Trade Hyperliquid perps through a local oppen instance over MCP. Use when the user asks to trade, check positions, manage orders, read market features, or supervise an oppen agent. Requires a paired oppen MCP server on localhost.
---

# oppen — trading through MCP

This skill ships when the MCP gateway lands (spec phase P4). It will cover:

- how to read `get_state` first, every time, and what its staleness flags mean
- the `reason` field: required on every action, rendered to the human as your claim
- the error taxonomy and what to do on each (`guardrail_reject` → adjust, never retry; `timeout_unknown_outcome` → `get_order_status(cloid)`, never resend)
- `preflight` before `place`; `get_features` instead of reasoning over raw books
- journaling with `remember` / `recall` so the next session knows why a position exists
- `set_alert` instead of polling

Until then, do not attempt to trade through oppen.
