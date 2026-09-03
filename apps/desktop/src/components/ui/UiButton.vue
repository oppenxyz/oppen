<script setup lang="ts">
withDefaults(
  defineProps<{
    variant?: "outline" | "primary" | "hazard";
    size?: "sm" | "md";
    block?: boolean;
  }>(),
  { variant: "outline", size: "md", block: false },
);
</script>

<template>
  <button type="button" class="btn" :class="[`btn--${variant}`, `btn--${size}`, { 'btn--block': block }]">
    <slot />
  </button>
</template>

<style scoped>
.btn {
  display: inline-flex;
  align-items: center;
  justify-content: center;
  border: 1px solid var(--rule-strong);
  font-family: var(--font-mono);
  font-size: var(--fs-label-lg);
  letter-spacing: var(--ls-button);
  text-transform: uppercase;
  white-space: nowrap;
  color: var(--body);
  background: transparent;
}

.btn--md {
  padding: 10px 14px;
}

.btn--sm {
  padding: var(--s-1) var(--s-2);
  font-size: var(--fs-label);
  letter-spacing: var(--ls-label-tight);
}

.btn--block {
  flex: 1;
  width: 100%;
}

.btn--outline:hover:not(:disabled) {
  border-color: var(--bracket);
  color: var(--signal);
}

.btn--primary {
  border-color: var(--signal);
  font-weight: 700;
  color: var(--void);
  background: var(--signal);
}

.btn--hazard {
  border-color: var(--hazard);
  color: var(--hazard);
}

.btn--hazard:hover:not(:disabled) {
  color: var(--void);
  background: var(--hazard);
}

.btn:disabled {
  opacity: 0.45;
}
</style>
