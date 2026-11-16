<script setup lang="ts">
import { withBase } from 'vitepress'

// Theme-aware screenshot: <Shot name="idle" alt="..." />
// Files come from scripts/copy-screenshots.mjs (public/screenshots/<name>-light|dark.png).
defineProps<{ name: string; alt: string }>()
</script>

<template>
  <figure class="shot">
    <img class="shot-light" :src="withBase(`/screenshots/${name}-light.png`)" :alt="alt" loading="lazy" />
    <img class="shot-dark" :src="withBase(`/screenshots/${name}-dark.png`)" :alt="alt" loading="lazy" />
  </figure>
</template>

<style scoped>
.shot {
  margin: 1.5rem 0;
}
.shot img {
  width: 100%;
  height: auto;
  border: 1px solid var(--vp-c-divider);
  border-radius: 8px;
}
.shot-dark {
  display: none;
}
:global(.dark) .shot-light {
  display: none;
}
:global(.dark) .shot-dark {
  display: block;
}
</style>
