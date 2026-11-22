<script setup lang="ts">
import { computed } from 'vue'
import { withBase } from 'vitepress'

// Theme-aware screenshot of the real app: <Shot name="idle" alt="..." caption="..." />.
// Files come from scripts/copy-screenshots.mjs (public/screenshots/<name>-light|dark.png, 1240x760).
// `focus="x,y,w,h"` (in those image pixels) shows only that part, for screens with a small centred
// panel such as the setup.
const props = defineProps<{ name: string; alt: string; caption?: string; focus?: string }>()
const W = 1240
const crop = computed(() => {
  if (!props.focus) return null
  const [x, y, w, h] = props.focus.split(',').map(Number)
  return {
    frame: { aspectRatio: `${w} / ${h}` },
    img: { width: `${(W / w) * 100}%`, maxWidth: 'none', marginLeft: `${(-x / w) * 100}%`, marginTop: `${(-y / w) * 100}%` }
  }
})
</script>

<template>
  <figure class="shot">
    <div class="shot__frame" :style="crop?.frame">
      <img class="shot-light" :style="crop?.img" :src="withBase(`/screenshots/${name}-light.png`)" :alt="alt" loading="lazy" />
      <img class="shot-dark" :style="crop?.img" :src="withBase(`/screenshots/${name}-dark.png`)" :alt="alt" loading="lazy" />
    </div>
    <figcaption v-if="caption">{{ caption }}</figcaption>
  </figure>
</template>

<style scoped>
.shot {
  margin: 24px 0 32px;
}
.shot__frame {
  overflow: hidden;
  border: 1px solid var(--vp-c-divider);
  border-radius: 10px;
  box-shadow: 0 1px 2px rgba(15, 20, 30, 0.04), 0 8px 24px -12px rgba(15, 20, 30, 0.18);
}
.shot img {
  display: block;
  width: 100%;
  height: auto;
}
.shot-dark {
  display: none !important;
}
:global(.dark) .shot-light {
  display: none !important;
}
:global(.dark) .shot-dark {
  display: block !important;
}
figcaption {
  margin-top: 10px;
  font-size: 0.8125rem;
  color: var(--vp-c-text-3);
  text-align: center;
}
</style>
