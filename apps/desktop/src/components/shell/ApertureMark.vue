<script setup lang="ts">
withDefaults(defineProps<{ size?: number }>(), { size: 16 });
</script>

<template>
  <svg class="mark" :width="size" :height="size" viewBox="0 0 100 100" fill="none" aria-hidden="true">
    <rect x="4" y="4" width="92" height="92" class="mark__plate" />
    <circle cx="50" cy="50" r="30" class="mark__hole" />
    <rect x="44" y="0" width="12" height="100" class="mark__hole" />
  </svg>
</template>

<style scoped>
.mark {
  display: block;
  flex: none;
}

.mark__plate {
  fill: var(--signal);
}

.mark__hole {
  fill: var(--void);
}
</style>
