<script setup lang="ts">
withDefaults(
  defineProps<{
    label: string;
    value?: string;
    tone?: "body" | "signal" | "uranium";
    size?: "sm" | "md";
  }>(),
  { value: "—", tone: "body", size: "sm" },
);
</script>

<template>
  <div class="stat" :class="`stat--${size}`">
    <div class="stat__label">{{ label }}</div>
    <div class="stat__value" :class="`stat__value--${tone}`">{{ value }}</div>
  </div>
</template>

<style scoped>
.stat__label {
  font-size: var(--fs-label);
  letter-spacing: var(--ls-label-tight);
  text-transform: uppercase;
  color: var(--bracket);
}

.stat__value {
  margin-top: var(--s-1);
}

.stat--sm .stat__value {
  font-size: var(--fs-body-sm);
}

.stat--md .stat__value {
  font-size: var(--fs-body);
}

.stat__value--body {
  color: var(--body);
}

.stat__value--signal {
  color: var(--signal);
}

.stat__value--uranium {
  color: var(--uranium);
}
</style>
