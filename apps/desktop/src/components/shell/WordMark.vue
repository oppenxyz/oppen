<script setup lang="ts">
withDefaults(
  defineProps<{
    size?: "nav" | "cover";
    /** Paints the mirrored E uranium — the cover's one live element. */
    accent?: boolean;
  }>(),
  { size: "nav", accent: false },
);
</script>

<template>
  <!-- A real E, mirrored with a transform, so the name still reads as OPPEN to assistive tech. -->
  <span class="wordmark" :class="`wordmark--${size}`" translate="no"
    ><span>OPP</span><span class="wordmark__e" :class="{ 'wordmark__e--accent': accent }">E</span><span>N</span></span
  >
</template>

<style scoped>
.wordmark {
  display: inline-flex;
  font-family: var(--font-mono);
  font-weight: 700;
  white-space: nowrap;
  color: var(--signal);
}

.wordmark--nav {
  font-size: var(--fs-display-xs);
  letter-spacing: var(--ls-display);
}

.wordmark--cover {
  justify-content: center;
  font-size: var(--fs-display-cover);
  line-height: 0.9;
  letter-spacing: -0.035em;
}

.wordmark__e {
  display: inline-block;
  transform: scaleX(-1);
}

.wordmark__e--accent {
  color: var(--uranium);
}
</style>
