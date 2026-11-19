<script setup lang="ts">
import { onMounted, ref, useId } from 'vue'
import { detectOs, type Os } from './os'

// Per-OS content: <OsTabs><template #mac>...</template><template #windows>...</template>
// <template #linux>...</template></OsTabs>. Opens on the visitor's OS; every tab stays reachable.
const tabs: { id: Os; label: string }[] = [
  { id: 'mac', label: 'macOS' },
  { id: 'windows', label: 'Windows' },
  { id: 'linux', label: 'Linux' }
]
const active = ref<Os>('mac')
// Stable between the server render and the browser, so the ids match after hydration.
const uid = useId()
onMounted(() => (active.value = detectOs()))

function key(e: KeyboardEvent, i: number) {
  const d = e.key === 'ArrowRight' ? 1 : e.key === 'ArrowLeft' ? -1 : 0
  if (!d) return
  const next = tabs[(i + d + tabs.length) % tabs.length]
  active.value = next.id
  ;(document.getElementById(`os-${uid}-${next.id}`) as HTMLElement | null)?.focus()
}
</script>

<template>
  <div class="os-tabs">
    <div class="os-tabs__bar" role="tablist" aria-label="Operating system">
      <button
        v-for="(t, i) in tabs"
        :id="`os-${uid}-${t.id}`"
        :key="t.id"
        role="tab"
        type="button"
        :aria-selected="active === t.id"
        :aria-controls="`os-${uid}-${t.id}-panel`"
        :tabindex="active === t.id ? 0 : -1"
        class="os-tabs__tab"
        @click="active = t.id"
        @keydown="key($event, i)"
      >
        {{ t.label }}
      </button>
    </div>
    <div
      v-for="t in tabs"
      v-show="active === t.id"
      :id="`os-${uid}-${t.id}-panel`"
      :key="t.id"
      role="tabpanel"
      :aria-labelledby="`os-${uid}-${t.id}`"
      class="os-tabs__panel"
    >
      <slot :name="t.id" />
    </div>
  </div>
</template>

<style scoped>
.os-tabs {
  margin: 24px 0;
  border: 1px solid var(--vp-c-divider);
  border-radius: 12px;
  background: var(--vp-c-bg);
}
.os-tabs__bar {
  display: flex;
  gap: 4px;
  padding: 6px;
  border-bottom: 1px solid var(--vp-c-divider);
  background: var(--vp-c-bg-alt);
  border-radius: 12px 12px 0 0;
}
.os-tabs__tab {
  padding: 6px 14px;
  border-radius: 8px;
  font-size: 0.875rem;
  font-weight: 500;
  color: var(--vp-c-text-2);
  transition: background-color 0.15s, color 0.15s;
}
.os-tabs__tab:hover {
  color: var(--vp-c-text-1);
}
.os-tabs__tab[aria-selected='true'] {
  background: var(--vp-c-bg);
  color: var(--vp-c-text-1);
  box-shadow: 0 1px 2px rgba(15, 20, 30, 0.08), 0 0 0 1px var(--vp-c-divider);
}
.os-tabs__tab:focus-visible {
  outline: 2px solid var(--vp-c-brand-2);
  outline-offset: 1px;
}
.os-tabs__panel {
  padding: 4px 20px 8px;
}
</style>
