<script setup lang="ts">
export type BracketCorner = "tl" | "tr" | "bl" | "br";

withDefaults(
  defineProps<{
    label?: string;
    meta?: string;
    /** Which corners carry the 14px bracket. Most housings carry none. */
    brackets?: readonly BracketCorner[];
    /** Inset housings carry their label inside the padding with no rule under it. */
    inset?: boolean;
    /** Pads the body of a non-inset housing. */
    pad?: boolean;
  }>(),
  { label: "", meta: "", brackets: () => [], inset: false, pad: false },
);
</script>

<template>
  <section class="housing" :class="{ 'housing--inset': inset }">
    <i v-for="corner in brackets" :key="corner" class="bracket" :class="`bracket--${corner}`" aria-hidden="true" />
    <header v-if="label || meta || $slots.label || $slots.meta" class="housing__head">
      <span class="housing__label"><slot name="label">{{ label }}</slot></span>
      <span class="housing__meta"><slot name="meta">{{ meta }}</slot></span>
    </header>
    <div class="housing__body" :class="{ 'housing__body--pad': pad }">
      <slot />
    </div>
    <footer v-if="$slots.footer" class="housing__foot">
      <slot name="footer" />
    </footer>
  </section>
</template>

<style scoped>
.housing {
  position: relative;
  display: flex;
  flex-direction: column;
  min-width: 0;
  min-height: 0;
  border: 1px solid var(--rule);
  background: var(--plate);
}

.housing--inset {
  padding: var(--s-3);
}

.housing__head {
  display: flex;
  flex: none;
  align-items: center;
  justify-content: space-between;
  gap: var(--s-3);
  height: var(--housing-head-h);
  padding: 0 var(--s-3);
  border-bottom: 1px solid var(--rule);
  font-size: var(--fs-label);
  letter-spacing: var(--ls-label);
  text-transform: uppercase;
  color: var(--bracket);
  white-space: nowrap;
}

.housing--inset > .housing__head {
  height: auto;
  padding: 0;
  border-bottom: 0;
}

.housing__label {
  display: flex;
  flex: 1;
  align-items: center;
  gap: var(--s-4);
  height: 100%;
  min-width: 0;
  overflow: hidden;
  text-overflow: ellipsis;
}

.housing__meta {
  display: flex;
  flex: none;
  align-items: center;
  gap: var(--s-2);
  min-width: 0;
  overflow: hidden;
  text-overflow: ellipsis;
}

.housing__meta:empty {
  display: none;
}

.housing__body {
  display: flex;
  flex: 1;
  flex-direction: column;
  min-width: 0;
  min-height: 0;
}

.housing__body--pad {
  padding: var(--s-3);
}

.housing--inset > .housing__head + .housing__body {
  margin-top: 10px;
}

.housing__foot {
  flex: none;
  padding: 10px var(--s-3);
  border-top: 1px solid var(--rule);
  font-size: var(--fs-label);
  line-height: 1.9;
  letter-spacing: var(--ls-label-tight);
  text-transform: uppercase;
  color: var(--bracket);
}

.bracket {
  position: absolute;
  z-index: 1;
  width: var(--bracket-size);
  height: var(--bracket-size);
  pointer-events: none;
}

.bracket--tl {
  top: -1px;
  left: -1px;
  border-top: 1px solid var(--bracket);
  border-left: 1px solid var(--bracket);
}

.bracket--tr {
  top: -1px;
  right: -1px;
  border-top: 1px solid var(--bracket);
  border-right: 1px solid var(--bracket);
}

.bracket--bl {
  bottom: -1px;
  left: -1px;
  border-bottom: 1px solid var(--bracket);
  border-left: 1px solid var(--bracket);
}

.bracket--br {
  right: -1px;
  bottom: -1px;
  border-right: 1px solid var(--bracket);
  border-bottom: 1px solid var(--bracket);
}
</style>
