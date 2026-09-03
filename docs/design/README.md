# oppen design system

Source of truth: `brand-book.dc.html` (open in a browser next to `support.js`). The v1 app mock is `app-mock-v1.dc.html` — six screens, HL-only, testnet-first. Both are live HTML: nav is clickable, values tick.

## Tokens

| Role | Value |
|---|---|
| ground / void | `#0A0B0C` |
| surface / plate | `#0D0F11` |
| edge / rule | `#24282C` |
| label / bracket | `#7A8188` |
| body | `#A9AFB5` |
| signal | `#E7E9EA` |
| uranium (accent) | `#FFD400` — ≤2% of any surface; `#C79E00` on light |
| hazard | `#FF4D2E` — liquidation and errors only |

Type: **Space Mono** 400/700 for display, numerals, labels and the character matrix; **Archivo** 400–600 for body and long copy. Numerals tabular by default. Labels 10–11px, +24% tracking, uppercase.

## Rules

- Four-tier contrast ladder: structure → content → primary → live. One live element per view. Skipping a tier is what makes the system look generic.
- Direction is carried by weight and outline, not by a green/red pair.
- No shadows, glows, gradients, or radius. Panels are housings: 1px rule, 14px bracket corners, tick ribbons only where data actually flows.
- Everything the brand draws is sampled onto a monospace grid first. Five-glyph ramp `+ * : - .` near → far; space is the sixth step. Never add a glyph.
- Motion is resampling, not easing. One 90ms clock, steps only, nothing translates or scales. Reduced motion = freeze on frame 0.
- Voice: short declaratives. Venue names, latencies and mechanics stated outright. "Agent" is a plain noun. No hype verbs, no emoji, no mascots, no safety claims we cannot audit, never a bomb joke.

## Motifs in the app

Character matrix (loading, empty states), stipple field (behind housings), tick ribbon (live data path), depth blocks (book texture, greyscale). ASCII gauges for cap utilization, funding countdown, approval TTL, exposure bars, margin.
