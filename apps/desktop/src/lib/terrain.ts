const clamp = (v: number) => Math.max(0, Math.min(1, v));

/** A continuous relief surface: broad landforms, folded ridges, then fine erosion.
 * Slow coordinate drift preserves geography between the 90ms samples. */
export interface TerrainPointer { x: number; y: number; amp: number }

export function reliefTerrainSample(x: number, y: number, t: number, pointer?: TerrainPointer): number {
  const u = x * 2 + t * .00032, v = y - t * .00014;
  const warpX = v * 5.2 + u * 1.7, warpY = u * 4.4 - v * 2.1;
  const wx = u + .13 * Math.sin(warpX), wy = v + .09 * Math.sin(warpY);
  const a = wx * 3.9 + Math.sin(wy * 4.2) * .65;
  const b = wy * 7.1 - wx * 1.5, c = wx * 9.3 + wy * 8.2, d = wx * 22 - wy * 17;
  const h = .62 * Math.sin(a) + .4 * Math.cos(b) + .18 * Math.sin(c) + .065 * Math.sin(d);
  // Analytic surface normal: one elevation evaluation instead of five per cell.
  const hx = 2.418 * Math.cos(a) + .6 * Math.sin(b) + 1.674 * Math.cos(c) + 1.43 * Math.cos(d);
  const hy = 1.6926 * Math.cos(a) * Math.cos(wy * 4.2) - 2.84 * Math.sin(b) + 1.476 * Math.cos(c) - 1.105 * Math.cos(d);
  const dx = hx * (1 + .221 * Math.cos(warpX)) + hy * .396 * Math.cos(warpY);
  const dy = hx * .676 * Math.cos(warpX) + hy * (1 - .189 * Math.cos(warpY));
  let light = clamp(.5 + (-dx * .55 - dy * .75 + 1.6) / Math.hypot(dx, dy, 2) * .5);
  // A survey light changes illumination and reveals detail at the original
  // coordinates. The terrain never bends or sheds water wakes.
  let inspection = 0;
  if (pointer && pointer.amp > 0) {
    const px = (pointer.x - x) * 2, py = pointer.y - y;
    const distance = Math.hypot(px, py), radius = .28;
    const edge = clamp(1 - distance / radius);
    inspection = clamp(pointer.amp) * edge * edge * (3 - 2 * edge);
    const lamp = clamp(.5 + (-dx * px - dy * py + .4) / (Math.hypot(dx, dy, 2) * Math.hypot(px, py, .2)) * .5);
    light += inspection * (lamp - light);
  }
  const major = Math.pow(Math.max(0, Math.cos(h * 6)), 18);
  const minor = Math.pow(Math.max(0, Math.cos(h * 42)), 10);
  const shoulder = Math.pow(Math.max(0, Math.cos(h * 42 - .65)), 5);
  const slope = Math.min(1, Math.hypot(dx, dy) / 7);
  // Hatching occupies slopes, leaving low basins and gaps between contours quiet.
  const hatch = Math.pow(Math.max(0, Math.sin(u * 104 + v * 79)), 4) * slope;
  const relief = clamp((h + .9) / 1.8);
  const surface = .025 + relief * .085 + hatch * light * .23;
  const fine = inspection > 0 ? Math.pow(Math.max(0, Math.cos(h * 84)), 10) : 0;
  const revealed = inspection * (fine * .35 + hatch * .22 + relief * .1);
  return clamp(surface + revealed + major * (.72 + light * .23)
    + minor * (.42 + light * .28) + shoulder * light * .13);
}
