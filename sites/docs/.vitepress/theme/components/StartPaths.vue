<script setup lang="ts">
import { withBase } from 'vitepress'

// The home page's "start here" routes: what people do with Siple, and what agents do.
const groups = [
  {
    title: 'Use Siple',
    lead: 'For anyone making and taking calls at a desk.',
    items: [
      { t: 'Quick start', d: 'Install, add your account and make your first call in five minutes.', l: '/guide/quick-start' },
      { t: 'Make and answer calls', d: 'Dial, answer, hold, mute and switch between lines.', l: '/guide/calls' },
      { t: 'Transfer a call', d: 'Hand a caller to a colleague, straight away or after a word first.', l: '/guide/transfers' },
      { t: 'See who is free', d: 'Busy lamps for your colleagues; click one to call or pick up.', l: '/guide/extensions' },
      { t: 'Fix a problem', d: 'No sound, not registering, and other common snags.', l: '/guide/troubleshooting' }
    ]
  },
  {
    title: 'Automate Siple',
    lead: 'For developers, testers and AI agents.',
    items: [
      { t: 'How agents drive Siple', d: 'Everything the window can do, a script or agent can do too.', l: '/agents/' },
      { t: 'Give an agent the skill', d: 'One command, no extra process: the agent uses siple-cli.', l: '/agents/skill' },
      { t: 'Connect over MCP', d: 'Add Siple to Claude Code or Claude Desktop in a minute.', l: '/agents/mcp-quickstart' },
      { t: 'API reference', d: 'Every method and event, with examples.', l: '/agents/api-reference' }
    ]
  }
]
</script>

<template>
  <section class="paths" aria-label="Start here">
    <div v-for="g in groups" :key="g.title" class="paths__group">
      <h2 class="paths__title">{{ g.title }}</h2>
      <p class="paths__lead">{{ g.lead }}</p>
      <ul class="paths__list">
        <li v-for="i in g.items" :key="i.l">
          <a :href="withBase(i.l)" class="paths__item">
            <span class="paths__text">
              <span class="paths__name">{{ i.t }}</span>
              <span class="paths__desc">{{ i.d }}</span>
            </span>
            <svg class="paths__arrow" aria-hidden="true" viewBox="0 0 16 16" width="16" height="16"><path d="M3 8h9M8.5 4.5 12 8l-3.5 3.5" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round" /></svg>
          </a>
        </li>
      </ul>
    </div>
  </section>
</template>

<style scoped>
.paths {
  display: grid;
  grid-template-columns: repeat(2, minmax(0, 1fr));
  gap: 24px;
}
@media (max-width: 760px) {
  .paths {
    grid-template-columns: 1fr;
    gap: 16px;
  }
  .paths__group {
    padding: 18px 16px 6px;
  }
}
.paths__group {
  padding: 24px 24px 12px;
  border: 1px solid var(--vp-c-divider);
  border-radius: 12px;
  background: var(--vp-c-bg);
}
.paths__title {
  margin: 0;
  font-size: 1.25rem;
  font-weight: 600;
  letter-spacing: -0.02em;
  border: 0;
  padding: 0;
}
.paths__lead {
  margin: 4px 0 12px;
  color: var(--vp-c-text-2);
  font-size: 0.9375rem;
}
.paths__list {
  list-style: none;
  margin: 0;
  padding: 0;
}
.paths__list li {
  border-top: 1px solid var(--vp-c-divider);
}
.paths__item {
  display: flex;
  align-items: center;
  gap: 16px;
  padding: 14px 4px;
  text-decoration: none;
  color: inherit;
}
.paths__text {
  display: flex;
  flex-direction: column;
  gap: 2px;
  flex: 1;
}
.paths__name {
  font-weight: 600;
  color: var(--vp-c-text-1);
}
.paths__desc {
  font-size: 0.875rem;
  color: var(--vp-c-text-2);
}
.paths__arrow {
  flex: none;
  color: var(--vp-c-text-3);
  transition: transform 0.2s cubic-bezier(0.16, 1, 0.3, 1), color 0.2s;
}
.paths__item:hover .paths__name {
  color: var(--vp-c-brand-1);
}
.paths__item:hover .paths__arrow {
  flex: none;
  color: var(--vp-c-brand-1);
  transform: translateX(3px);
}
.paths__item:focus-visible {
  outline: 2px solid var(--vp-c-brand-2);
  outline-offset: 2px;
  border-radius: 6px;
}
@media (prefers-reduced-motion: reduce) {
  .paths__arrow {
    transition: none;
  }
}
</style>
