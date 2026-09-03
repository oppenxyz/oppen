<script setup lang="ts">
export interface ReadoutRow {
  k: string;
  v: string;
  tone?: "body" | "signal" | "uranium" | "hazard";
}

withDefaults(
  defineProps<{
    rows: readonly ReadoutRow[];
    size?: "sm" | "md";
  }>(),
  { size: "sm" },
);
</script>

<template>
  <dl class="readout" :class="`readout--${size}`">
    <div v-for="row in rows" :key="row.k" class="readout__row">
      <dt class="readout__k">{{ row.k }}</dt>
      <dd class="readout__v" :class="`readout__v--${row.tone ?? 'body'}`">
        <slot name="value" :row="row">{{ row.v }}</slot>
      </dd>
    </div>
  </dl>
</template>

<style scoped>
.readout {
  letter-spacing: 0.04em;
}

.readout--sm {
  font-size: var(--fs-body-sm);
  line-height: 2;
}

.readout--md {
  font-size: var(--fs-body);
  line-height: 2.2;
}

.readout__row {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: var(--s-3);
  border-bottom: 1px solid var(--rule);
}

.readout__row:last-child {
  border-bottom: 0;
}

.readout__k {
  color: var(--bracket);
}

.readout__v {
  display: flex;
  align-items: center;
  gap: var(--s-2);
  text-align: right;
}

.readout__v--body {
  color: var(--body);
}

.readout__v--signal {
  color: var(--signal);
}

.readout__v--uranium {
  color: var(--uranium);
}

.readout__v--hazard {
  color: var(--hazard);
}
</style>
