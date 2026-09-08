<script setup lang="ts">
/**
 * The candle panel (`docs/specs/charts.md` §2).
 *
 * The renderer in `lib/candles.ts` does all the drawing and knows nothing
 * about the DOM: it paints a character grid plus a parallel grid of one-letter
 * ink codes, and this component's only job is to turn each row's runs into
 * spans and map an ink code to a design token. That split is why the renderer
 * has a golden-frame test at all — a frame is comparable because it is text.
 *
 * The grid is measured from the element rather than fixed, so the chart fills
 * whatever the layout gives it. `renderCandles` clamps its own geometry, which
 * is what keeps a measurement taken before the mono font loads — character
 * width zero, so `cols` is `Infinity` — from allocating a grid with no bound.
 */
import { computed, onBeforeUnmount, onMounted, ref, watch } from "vue";

import { INK, createCandleRenderer, frameRuns, type CandleFrame } from "../lib/candles";
import type { ChartData } from "../stores/market";

const props = defineProps<{ data: ChartData | null; observationSummary?: string }>();

/** Ink code to design token. The renderer emits codes so it can stay CSS-free. */
const TOKEN: Record<string, string> = {
  [INK.up]: "var(--up)",
  [INK.down]: "var(--down)",
  [INK.volume]: "var(--body-dim)",
  [INK.rule]: "var(--rule)",
  [INK.label]: "var(--bracket)",
  [INK.last]: "var(--uranium)",
};

const host = ref<HTMLElement | null>(null);
const cell = ref<HTMLElement | null>(null);
const cols = ref(0);
const width = ref(0);
const rows = ref(0);
const frame = ref<CandleFrame | null>(null);
const renderer = createCandleRenderer();

/**
 * Measure the grid from a hidden single-character probe rather than assuming a
 * ratio. A mono font's advance width is a font fact, not a constant, and
 * guessing it puts the axis labels a column off at some zoom levels.
 */
function measure(): void {
  const box = host.value;
  const probe = cell.value;
  if (!box || !probe) return;
  const charWidth = probe.getBoundingClientRect().width;
  const lineHeight = probe.getBoundingClientRect().height;
  if (charWidth <= 0 || lineHeight <= 0) return;
  // Four characters per candle slot: three body columns and a gap.
  width.value = Math.floor(box.clientWidth / charWidth);
  cols.value = Math.floor(width.value / 4);
  rows.value = Math.floor(box.clientHeight / lineHeight);
  paint();
}

function paint(): void {
  const data = props.data;
  if (!data || cols.value <= 0 || rows.value <= 0) {
    frame.value = null;
    return;
  }
  const painted = renderer.render({
    closed: data.closed,
    forming: data.forming,
    cols: cols.value,
    maxWidth: width.value,
    rows: rows.value,
    intervalMs: data.intervalMs,
    priceDecimals: data.priceDecimals ?? 6,
    latestTrade: data.latestTrade,
    tzOffsetMinutes: -new Date().getTimezoneOffset(),
  });
  // `changed: false` means the cells are identical to the last frame, so the
  // spans below are already correct and reassigning would churn the DOM for
  // nothing. That diff is the whole reason the renderer owns a buffer.
  if (painted.changed || frame.value === null) frame.value = painted.frame;
}

/** One array of runs per row, which is what the template iterates. */
const lines = computed(() => (frame.value ? frameRuns(frame.value) : []));
const summary = computed(() => {
  const data = props.data;
  const last = data?.forming ?? data?.closed[data.closed.length - 1];
  const bar = last ? `Last candle close ${last.close}. High ${last.high}, low ${last.low}. ${data!.forming ? 'Last candle is forming.' : 'Last bucket elapsed; completeness is separate.'}` : "No bars observed.";
  return `Candlestick chart. ${bar} ${props.observationSummary ?? ""}`;
});

/**
 * Why the panel is blank, in the renderer's own words. Three different
 * failures paint the same empty grid, and a panel that cannot tell them apart
 * reads as downtime — which `charts.md` §3.2 names as the failure to avoid.
 */
const blank = computed(() => {
  if (props.data === null) return "No chart observations yet.";
  switch (frame.value?.status) {
    case "no_bars":
      return "No bars in the accepted chart observation.";
    case "no_finite_bars":
      return "Every bar in this window was unreadable.";
    case "grid_too_small":
      return "The panel is too small to carry an axis.";
    default:
      return null;
  }
});

let observer: ResizeObserver | null = null;

onMounted(() => {
  measure();
  observer = new ResizeObserver(measure);
  if (host.value) observer.observe(host.value);
  document.fonts.addEventListener("loadingdone", measure);
  void document.fonts.ready.then(() => { if (observer) measure(); });
});

onBeforeUnmount(() => {
  observer?.disconnect();
  observer = null;
  document.fonts.removeEventListener("loadingdone", measure);
});

watch(() => props.data, paint);
</script>

<template>
  <div ref="host" class="candles">
    <span ref="cell" class="candles__probe" aria-hidden="true">M</span>
    <p v-if="blank" class="candles__blank">{{ blank }}</p>
    <pre v-else class="candles__grid" role="img" :aria-label="summary"><span
      v-for="(runs, y) in lines"
      :key="y"
      class="candles__line"
    ><span
      v-for="(run, x) in runs"
      :key="x"
      :style="{ color: TOKEN[run.ink] ?? 'var(--body)' }"
    >{{ run.text }}</span>
</span></pre>
  </div>
</template>

<style scoped>
.candles {
  position: relative;
  block-size: 100%;
  inline-size: 100%;
  overflow: hidden;
}

/* Measured, never shown. `position: absolute` keeps it out of the flow so the
   grid gets the panel's full height. */
.candles__probe {
  position: absolute;
  visibility: hidden;
  font-family: Menlo, Consolas, "Liberation Mono", monospace;
  font-size: var(--fs-body-sm);
  line-height: 1.1;
}

.candles__grid {
  margin: 0;
  font-family: Menlo, Consolas, "Liberation Mono", monospace;
  font-size: var(--fs-body-sm);
  line-height: 1.1;
  white-space: pre;
  color: var(--body);
}

.candles__blank {
  margin: 0;
  padding: var(--s-2);
  font-size: var(--fs-body-sm);
  color: var(--body-dim);
}
</style>
