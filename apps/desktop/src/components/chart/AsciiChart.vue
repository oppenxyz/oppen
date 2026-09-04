<script setup lang="ts">
/**
 * The candle chart surface.
 *
 * WHY a character grid: `decisions.md` U1 makes ASCII the primary renderer and U2
 * drops the canvas fallback, so this component is the chart. It owns no market state
 * and no timer — `specs/charts.md` §2.4 puts bar assembly in the core, and §2.5 and
 * §5.4 put every animated surface on the one 90 ms clock in `src/lib/clock.ts`.
 *
 * The clock is a resampling signal, not an animation: on each tick the component asks
 * the renderer for a frame, and the renderer answers whether the frame differs from
 * the last one. An identical frame skips the DOM write entirely, which is the common
 * case between trades on a long interval.
 */
import { computed, shallowRef, watchEffect } from "vue";
import {
  createCandleRenderer,
  frameRuns,
  INK,
  type Bar,
  type FrameStatus,
  type InkRun,
} from "../../lib/candles";
import { useClock } from "../../lib/clock";

const props = withDefaults(
  defineProps<{
    /** Closed bars, oldest first, assembled by the core. */
    closed: readonly Bar[];
    /** The in-progress bar, drawn with the forming glyph. */
    forming?: Bar | null;
    /** Candle slots across the plot. Each slot is four characters wide. */
    cols?: number;
    /** Total grid rows, axis rule and volume and time labels included. */
    rows?: number;
    /** Bar interval in milliseconds; selects the X-axis label family. */
    intervalMs: number;
    /** `max_price_decimals` for the asset. Labels never carry more. */
    priceDecimals?: number;
    /** Minutes east of UTC for time labels. Explicit so a frame never depends on the host. */
    tzOffsetMinutes?: number;
    /** Venue symbol, used only for the accessible name. */
    symbol?: string;
  }>(),
  {
    forming: null,
    cols: 34,
    rows: 24,
    priceDecimals: 2,
    tzOffsetMinutes: 0,
    symbol: "",
  },
);

/** Ink code to token class. The renderer emits codes so it stays free of CSS. */
const INK_CLASS: Readonly<Record<string, string>> = {
  [INK.up]: "ink-up",
  [INK.down]: "ink-down",
  [INK.volume]: "ink-volume",
  [INK.rule]: "ink-rule",
  [INK.label]: "ink-label",
  [INK.last]: "ink-last",
};

function inkClass(code: string): string {
  return INK_CLASS[code] ?? "ink-none";
}

/**
 * Why the plot is empty, in words. `specs/charts.md` §3.2 is explicit that an empty
 * region must say so and must never read as downtime, so a blank grid is not an option:
 * the three blank causes are distinguishable in the frame's status and each one gets a
 * sentence. The copy lives here and never in the renderer.
 *
 * §3.2's fuller wording is "no local history before HH:MM". The timestamp half needs the
 * local aggregator's start instant, which this component is not given and must not
 * invent — a prop for it belongs with the interval resolver. What is fixed here is the
 * part that misleads: the panel now states its own emptiness instead of showing a box.
 */
const STATUS_TEXT: Readonly<Record<FrameStatus, string>> = {
  ok: "",
  no_bars: "no bars for this interval yet",
  no_finite_bars: "no usable bars in this window",
  grid_too_small: "panel too small to draw an axis",
};

const renderer = createCandleRenderer();
const lines = shallowRef<InkRun[][]>([]);
const status = shallowRef<FrameStatus>("no_bars");
const tick = useClock();
let painted = false;

watchEffect(() => {
  // Resample on the shared clock. Reading the tick is the whole subscription; this
  // component must never own a timer of its own (§5.4).
  void tick.value;
  const { frame, changed } = renderer.render({
    closed: props.closed,
    forming: props.forming,
    cols: props.cols,
    rows: props.rows,
    intervalMs: props.intervalMs,
    priceDecimals: props.priceDecimals,
    tzOffsetMinutes: props.tzOffsetMinutes,
  });
  if (!changed && painted) return;
  painted = true;
  status.value = frame.status;
  lines.value = frame.status === "ok" ? frameRuns(frame) : [];
});

const description = computed(() => {
  if (status.value !== "ok") return `${props.symbol || "candle"} chart, ${STATUS_TEXT[status.value]}`;
  const last = props.forming ?? props.closed[props.closed.length - 1];
  const price = last ? last.close.toFixed(props.priceDecimals) : "no data";
  return `${props.symbol || "candle"} chart, last ${price}`;
});
</script>

<template>
  <div class="chart" role="img" :aria-label="description">
    <div
      v-for="(runs, y) in lines"
      :key="y"
      class="chart__row"
    ><span
      v-for="(run, index) in runs"
      :key="index"
      :class="inkClass(run.ink)"
    >{{ run.text }}</span></div>
    <p v-if="status !== 'ok'" class="chart__empty">{{ STATUS_TEXT[status] }}</p>
  </div>
</template>

<style scoped>
.chart {
  font-family: var(--font-mono);
  font-size: var(--fs-body);
  font-variant-numeric: tabular-nums;
  line-height: 1.1;
  letter-spacing: 0;
  color: var(--body-dim);
}

.chart__row {
  white-space: pre;
}

/* Direction is carried by the glyph as well as the ink, so these two are a second
   channel and never the only one (specs/charts.md §2.1, decisions.md U3). The
   fallbacks keep the chart correct if the tokens are ever stripped from a build. */
.ink-up {
  color: var(--up, #2fbf71);
}

.ink-down {
  color: var(--down, #e5484d);
}

/* Volume carries no direction: colouring it doubles the ink for information already
   carried twice above it (§2.3). */
.ink-volume {
  color: var(--body-dim);
}

.ink-rule {
  color: var(--rule);
}

.ink-label {
  color: var(--bracket);
}

/* The last price is the one live element on the surface, so it takes the accent. */
.ink-last {
  color: var(--uranium);
}

.ink-none {
  color: inherit;
}

/* An empty plot states why. Label ink, not hazard: nothing here is wrong, there is
   simply nothing to draw, and hazard keeps its exclusive meaning. */
.chart__empty {
  margin: 0;
  color: var(--bracket);
}
</style>
