<script setup lang="ts">
/**
 * The walkthrough overlay: dims the console, cuts a hole around the element
 * the current step is about, and puts a callout beside it.
 *
 * The hole is a real hole — a box-shadow spread large enough to cover the
 * viewport, cast *outward* from the target's rectangle. That keeps one
 * element at full brightness without re-parenting it, so the thing being
 * explained is the live control and not a screenshot of one.
 */
import { computed, nextTick, onBeforeUnmount, onMounted, ref, watch } from "vue";

import { currentStep, endTour, isOpen, nextStep, prevStep, TOUR, tour } from "../../stores/tour";
import UiButton from "../ui/UiButton.vue";

interface Rect {
  top: number;
  left: number;
  width: number;
  height: number;
}

const rect = ref<Rect | null>(null);
const dialog = ref<HTMLElement | null>(null);
const card = ref<HTMLElement | null>(null);
const cardSize = ref({ width: 380, height: 190 });
let returnFocus: HTMLElement | null = null;
let cardObserver: ResizeObserver | null = null;
watch(isOpen, async (open) => {
  if (open) {
    // WebKit may leave focus on body after a mouse click. Body is not a
    // useful return target; use the persistent Setup control in that case.
    returnFocus = document.activeElement instanceof HTMLElement && document.activeElement !== document.body
      ? document.activeElement : null;
    await nextTick();
    dialog.value?.querySelector<HTMLButtonElement>("button")?.focus();
    cardObserver = new ResizeObserver(() => {
      if (card.value) cardSize.value = { width: card.value.offsetWidth, height: card.value.offsetHeight };
    });
    if (card.value) cardObserver.observe(card.value);
  } else {
    cardObserver?.disconnect();
    await nextTick();
    (returnFocus?.isConnected ? returnFocus : document.querySelector<HTMLElement>('[data-tour="setup"]'))?.focus();
  }
});
/** Padding around the target so the highlight does not clip its own border. */
const PAD = 6;

function measure(): void {
  const step = currentStep.value;
  if (!step) {
    rect.value = null;
    return;
  }
  const node = document.querySelector(`[data-tour="${step.target}"]`);
  if (!(node instanceof HTMLElement)) {
    // A step whose target is not on screen still shows its callout, centred.
    // Silently skipping would make the tour's length depend on which panels
    // happen to be mounted, and an operator counting steps would be misled.
    rect.value = null;
    return;
  }
  const box = node.getBoundingClientRect();
  rect.value = {
    top: box.top - PAD,
    left: box.left - PAD,
    width: box.width + PAD * 2,
    height: box.height + PAD * 2,
  };
}

/** Re-measure after the view switch the step asked for has rendered. */
watch(currentStep, async () => {
  await nextTick();
  measure();
});

function onKey(event: KeyboardEvent): void {
  if (!isOpen.value) return;
  if (event.key === "Tab") {
    const buttons = [...(dialog.value?.querySelectorAll<HTMLButtonElement>("button:not(:disabled)") ?? [])];
    const index = buttons.indexOf(document.activeElement as HTMLButtonElement);
    const next = event.shiftKey ? (index <= 0 ? buttons.length - 1 : index - 1) : (index + 1) % buttons.length;
    event.preventDefault();
    buttons[next]?.focus();
    return;
  }
  // Enter belongs to the focused native button; a global handler would advance twice.
  if (["Escape", "ArrowRight", "ArrowLeft"].includes(event.key)) event.preventDefault();
  if (event.key === "Escape") endTour();
  if (event.key === "ArrowRight") nextStep();
  if (event.key === "ArrowLeft") prevStep();
}

onMounted(() => {
  window.addEventListener("resize", measure);
  window.addEventListener("scroll", measure, true);
  window.addEventListener("keydown", onKey);
  measure();
});

onBeforeUnmount(() => {
  cardObserver?.disconnect();
  window.removeEventListener("resize", measure);
  window.removeEventListener("scroll", measure, true);
  window.removeEventListener("keydown", onKey);
});

const holeStyle = computed(() => {
  const box = rect.value;
  if (!box) return { display: "none" };
  return {
    top: `${box.top}px`,
    left: `${box.left}px`,
    width: `${box.width}px`,
    height: `${box.height}px`,
  };
});

/**
 * Where the callout goes. The step names a side; this keeps the card inside
 * the viewport when that side has no room, because a callout half off-screen
 * explains nothing.
 */
const calloutStyle = computed(() => {
  const box = rect.value;
  const step = currentStep.value;
  if (!box || !step) {
    return { top: "50%", left: "50%", transform: "translate(-50%, -50%)" };
  }
  const GAP = 14;
  const W = cardSize.value.width;
  const H = cardSize.value.height;
  let top = box.top;
  let left = box.left;

  switch (step.placement) {
    case "bottom":
      top = box.top + box.height + GAP;
      left = box.left;
      break;
    case "top":
      top = box.top - H - GAP;
      left = box.left;
      break;
    case "right":
      top = box.top;
      left = box.left + box.width + GAP;
      break;
    case "left":
      top = box.top;
      left = box.left - W - GAP;
      break;
  }

  const maxLeft = window.innerWidth - W - GAP;
  const maxTop = window.innerHeight - H - GAP;
  return {
    top: `${Math.max(GAP, Math.min(top, maxTop))}px`,
    left: `${Math.max(GAP, Math.min(left, maxLeft))}px`,
  };
});

const position = computed(() => `${tour.step + 1} / ${TOUR.length}`);
const isLast = computed(() => tour.step === TOUR.length - 1);
</script>

<template>
  <div v-if="isOpen" ref="dialog" class="tour" role="dialog" aria-modal="true" aria-label="oppen walkthrough">
    <div class="tour__scrim" :class="{ 'tour__scrim--fallback': !rect }" @click="endTour" />
    <div class="tour__hole" :style="holeStyle" aria-hidden="true" />

    <div v-if="currentStep" ref="card" class="tour__card" :style="calloutStyle">
      <div class="tour__head">
        <span class="tour__pos">{{ position }}</span>
        <button type="button" class="tour__skip" @click="endTour">Skip</button>
      </div>
      <h2 class="tour__title">{{ currentStep.title }}</h2>
      <p class="tour__body">{{ currentStep.body }}</p>
      <p v-if="currentStep.caveat" class="tour__caveat">Not yet: {{ currentStep.caveat }}</p>
      <div class="tour__actions">
        <UiButton v-if="tour.step > 0" @click="prevStep">Back</UiButton>
        <UiButton variant="primary" @click="nextStep">{{ isLast ? "Done" : "Next →" }}</UiButton>
      </div>
    </div>
  </div>
</template>

<style scoped>
.tour {
  position: fixed;
  inset: 0;
  z-index: 90;
}

.tour__scrim {
  position: absolute;
  inset: 0;
}

.tour__scrim--fallback { background: rgb(10 11 12 / 78%); }

/* The spotlight: an outward shadow big enough to cover any viewport, so the
   target keeps its own live rendering rather than being copied. */
.tour__hole {
  position: absolute;
  border: 1px solid var(--uranium);
  box-shadow: 0 0 0 100vmax rgb(10 11 12 / 78%);
  pointer-events: none;
}

.tour__card {
  position: absolute;
  width: min(380px, calc(100vw - 28px));
  max-height: calc(100vh - 28px);
  overflow-y: auto;
  padding: var(--s-4);
  border: 1px solid var(--rule-strong);
  background: var(--plate);
}

.tour__head {
  display: flex;
  align-items: center;
  justify-content: space-between;
}

.tour__pos {
  font-size: var(--fs-label);
  letter-spacing: var(--ls-chip);
  color: var(--bracket);
}

.tour__skip {
  border: 0;
  background: none;
  font: inherit;
  font-size: var(--fs-label);
  letter-spacing: var(--ls-chip);
  color: var(--bracket);
  cursor: pointer;
  text-transform: uppercase;
}

.tour__skip:hover {
  color: var(--signal);
}

.tour__title {
  margin: var(--s-3) 0 var(--s-2);
  font-family: var(--font-sans);
  font-size: var(--fs-display-sm);
  letter-spacing: var(--ls-display);
  color: var(--signal);
}

.tour__body {
  margin: 0;
  font-size: var(--fs-copy-sm);
  line-height: 1.5;
  color: var(--body);
}

.tour__caveat {
  margin: var(--s-3) 0 0;
  padding-left: var(--s-3);
  border-left: 1px solid var(--uranium);
  font-size: var(--fs-body);
  line-height: 1.45;
  color: var(--body-dim);
}

.tour__actions {
  display: flex;
  justify-content: flex-end;
  gap: var(--s-2);
  margin-top: var(--s-4);
}
</style>
