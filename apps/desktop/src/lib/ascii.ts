/**
 * Character-grid primitives, ported from docs/design/app-mock-v1.dc.html.
 *
 * Everything the brand draws is sampled onto a monospace grid. The five-glyph
 * ramp runs near → far; space is the sixth step. Never add a glyph.
 */

export const RAMP = ["+", "*", ":", "-", "."] as const;

export const MATRIX_W = 44;
export const MATRIX_H = 22;

export type MatrixMode = "breathe" | "sweep";

/** The 44×22 aperture character matrix at clock tick `t`. */
export function matrix(t: number, mode: MatrixMode = "breathe"): string {
  const r = 0.3 + (mode === "breathe" ? 0.022 * Math.sin(t / 7) : 0);
  const sweep = mode === "sweep" ? (t * 0.14) % (Math.PI * 2) : null;
  const rows: string[] = [];

  for (let y = 0; y < MATRIX_H; y++) {
    let row = "";
    for (let x = 0; x < MATRIX_W; x++) {
      const nx = (x + 0.5) / MATRIX_W;
      const ny = (y + 0.5) / MATRIX_H;
      const dx = nx - 0.5;
      const dy = ny - 0.5;
      const d = Math.sqrt(dx * dx + dy * dy);

      if (nx < 0.04 || nx > 0.96 || ny < 0.04 || ny > 0.96 || d < r || Math.abs(dx) < 0.06) {
        row += " ";
        continue;
      }

      let f = Math.min(1, Math.abs(d - r) / 0.34);
      if (sweep !== null) {
        let a = Math.atan2(dy, dx);
        if (a < 0) a += Math.PI * 2;
        let ad = Math.abs(a - sweep);
        if (ad > Math.PI) ad = Math.PI * 2 - ad;
        f = Math.max(0, Math.min(1, f - Math.max(0, 1 - ad / 0.6) * 0.85 + 0.25));
      }
      row += RAMP[Math.min(4, Math.floor(f * 5))];
    }
    rows.push(row.replace(/\s+$/, ""));
  }

  return rows.join("\n");
}

/** An `n`-cell ASCII gauge filled to `v` in [0, 1]. Unfilled cells are spaces. */
export function gauge(v: number, n: number): string {
  const filled = Math.max(0, Math.min(n, Math.round(v * n)));
  let out = "";
  for (let i = 0; i < n; i++) {
    out += i < filled ? RAMP[Math.min(4, Math.floor((i / Math.max(filled, 1)) * 5))] : " ";
  }
  return out;
}

/** The first `chars` characters of a typed boot log, with a block cursor on the active line. */
export function boot(lines: readonly string[], chars: number): string {
  const out: string[] = [];
  let left = chars;
  for (const line of lines) {
    if (left <= 0) break;
    out.push(left >= line.length ? line : line.slice(0, left) + "█");
    left -= line.length + 4;
  }
  return out.join("\n");
}
