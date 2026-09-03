<script setup lang="ts">
import type { MatrixMode } from "../../lib/ascii";
import CharacterMatrix from "../ascii/CharacterMatrix.vue";

withDefaults(
  defineProps<{
    /** One short declarative line. */
    line: string;
    /** Only the view's one live housing carries the matrix. */
    matrix?: boolean;
    mode?: MatrixMode;
    size?: "xs" | "sm" | "md" | "lg" | "xl";
  }>(),
  { matrix: false, mode: "breathe", size: "sm" },
);
</script>

<template>
  <div class="empty" :class="{ 'empty--matrix': matrix }">
    <CharacterMatrix v-if="matrix" :mode="mode" :size="size" />
    <p class="empty__line">{{ line }}</p>
  </div>
</template>

<style scoped>
.empty {
  display: flex;
  flex: 1;
  flex-direction: column;
  align-items: center;
  justify-content: center;
  gap: var(--s-4);
  min-height: 0;
  padding: var(--s-4) var(--s-3);
  overflow: hidden;
  text-align: center;
}

.empty__line {
  font-size: var(--fs-body);
  letter-spacing: 0.04em;
  color: var(--body);
}
</style>
