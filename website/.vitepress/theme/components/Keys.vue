<script setup lang="ts">
import { computed, onMounted, ref } from 'vue'
import { detectOs } from './os'

// A keyboard shortcut, shown for the visitor's OS: <Keys combo="Mod+Shift+A" />.
// "Mod" is Cmd on macOS and Ctrl elsewhere (as in config.toml).
const props = defineProps<{ combo: string }>()
const mac = ref(false)
onMounted(() => (mac.value = detectOs() === 'mac'))

const MAC: Record<string, string> = { mod: '⌘', cmd: '⌘', shift: '⇧', alt: '⌥', ctrl: '⌃', enter: '↩' }
const keys = computed(() =>
  props.combo.split('+').map((k) => {
    const low = k.toLowerCase()
    if (mac.value && MAC[low]) return MAC[low]
    if (low === 'mod') return 'Ctrl'
    return k.length === 1 ? k.toUpperCase() : k
  })
)
const spoken = computed(() => keys.value.join(' + '))
</script>

<template>
  <span class="siple-keys" :aria-label="spoken" :title="spoken"
    ><template v-for="(k, i) in keys" :key="i"
      ><span v-if="i > 0 && !mac" class="plus" aria-hidden="true">+</span><kbd aria-hidden="true">{{ k }}</kbd></template
    ></span
  >
</template>

<style scoped>
.siple-keys {
  white-space: nowrap;
}
.siple-keys kbd + kbd {
  margin-left: 2px;
}
.plus {
  margin: 0 2px;
  color: var(--vp-c-text-3);
  font-size: 0.85em;
}
</style>
