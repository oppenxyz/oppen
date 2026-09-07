<script setup lang="ts">
import UiButton from "../ui/UiButton.vue";
import type { MatrixMode } from "../../lib/ascii";
import CharacterMatrix from "../ascii/CharacterMatrix.vue";

withDefaults(
  defineProps<{
    /** One short declarative line. */
    line: string;
    /** Only the view's one live housing carries the matrix. */
    matrix?: boolean;
    /** Only true while the source read is actually in flight. */
    reading?: boolean;
    action?: string;
    mode?: MatrixMode;
    size?: "xs" | "sm" | "md" | "lg" | "xl";
  }>(),
  { matrix: false, mode: "breathe", size: "sm" },
);
defineEmits<{ action: [] }>();
</script>

<template>
  <div class="empty" :class="{ 'empty--matrix': matrix }" :aria-busy="reading || undefined">
    <CharacterMatrix v-if="matrix" :static="!reading" :mode="reading ? 'sweep' : mode" :size="size" />
    <p class="empty__line">{{ reading ? 'Reading records…' : line }}</p>
    <UiButton v-if="action" @click="$emit('action')">{{ action }}</UiButton>
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
