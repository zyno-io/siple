import { defineConfig } from 'vitepress'

const REPO = 'https://github.com/zyno-io/siple'

// Project Pages live under /siple/. For a custom domain (docs.example.com) set BASE to '/'
// and change SITE_URL, and add a CNAME file under website/public/.
const BASE = '/siple/'
const SITE_URL = 'https://zyno-io.github.io/siple/'

// Repo files whose links should land on a site page instead of GitHub.
const ROUTES: Record<string, string> = {
  'CONFIG.md': '/reference/configuration',
  'API.md': '/agents/api-reference',
  'API-intro.md': '/agents/api-reference',
  'ARCHITECTURE.md': '/reference/architecture',
  'CONTRIBUTING.md': '/project/contributing',
  'SECURITY.md': '/project/security',
  'CHANGELOG.md': '/project/changelog'
}

// The included files (docs/*.md, CONTRIBUTING.md, ...) use repo-relative links so that they work
// on GitHub. Rewrite them while rendering: known docs go to site routes (anchors kept), any other
// repo-relative path goes to the file on GitHub. Hand-written site pages must use absolute links
// (`/guide/calls`), which this rule leaves alone.
function rewriteRepoLinks(md: any) {
  md.core.ruler.push('siple-repo-links', (state: any) => {
    for (const block of state.tokens) {
      if (block.type !== 'inline' || !block.children) continue
      for (const t of block.children) {
        if (t.type !== 'link_open') continue
        const href: string | null = t.attrGet('href')
        if (!href || /^([a-z][a-z0-9+.-]*:|#|\/)/i.test(href)) continue
        const [path, hash = ''] = href.split('#')
        const clean = path.replace(/^(\.\.?\/)+/, '')
        const file = clean.split('/').pop() ?? ''
        const anchor = hash ? `#${hash}` : ''
        if (ROUTES[file]) {
          t.attrSet('href', ROUTES[file] + anchor)
        } else {
          const inDocs = !clean.includes('/') && clean.endsWith('.md') && file !== 'CODE_OF_CONDUCT.md'
          t.attrSet('href', `${REPO}/blob/main/${inDocs ? 'docs/' : ''}${clean}${anchor}`)
        }
      }
    }
  })
}

// The generated API reference (and a few docs) mention placeholders such as <target>@<domain> in
// running text. GitHub hides unknown tags; Vue would fail the build on them. Show them as text.
const HTML_TAGS = new Set(
  'a abbr b blockquote br code del details div em h1 h2 h3 h4 h5 h6 hr i img kbd li ol p pre s span strong sub summary sup table tbody td th thead tr u ul Shot'.split(
    ' '
  )
)
function escapeUnknownTags(md: any) {
  md.core.ruler.push('siple-unknown-tags', (state: any) => {
    for (const block of state.tokens) {
      if (block.type !== 'inline' || !block.children) continue
      for (const t of block.children) {
        if (t.type !== 'html_inline') continue
        const m = /^<\/?([A-Za-z][\w-]*)/.exec(t.content)
        if (m && !HTML_TAGS.has(m[1])) t.type = 'text'
      }
    }
  })
}

export default defineConfig({
  title: 'Siple',
  description:
    'Siple is a source-available desktop SIP softphone for macOS, Windows and Linux, with first-class AI-agent control (JSON-RPC API and MCP server).',
  lang: 'en-US',
  base: BASE,
  cleanUrls: true,
  // Dead-link detection is the docs link check in CI: keep it on.
  ignoreDeadLinks: false,
  // Enable only where full git history is available (CI checks out with fetch-depth: 0).
  lastUpdated: process.env.DOCS_LAST_UPDATED === '1',
  head: [['link', { rel: 'icon', type: 'image/png', href: `${BASE}logo.png` }]],
  sitemap: { hostname: SITE_URL },
  // The local search index of the full API reference is a large chunk by design.
  vite: { build: { chunkSizeWarningLimit: 2500 } },
  markdown: {
    // GitHub-style heading ids (`config.edit` -> `configedit`), so anchors in the included files
    // (API.md#configedit, ...) work on GitHub and on the site.
    anchor: {
      slugify: (s: string) =>
        s
          .trim()
          .toLowerCase()
          .replace(/[^\p{L}\p{N}\p{M}\s_-]/gu, '')
          .replace(/\s/g, '-')
    },
    config(md) {
      rewriteRepoLinks(md)
      escapeUnknownTags(md)
    }
  },
  themeConfig: {
    logo: '/logo.png',
    siteTitle: 'Siple',
    nav: [
      { text: 'Guide', link: '/guide/install', activeMatch: '^/guide/' },
      { text: 'AI agents', link: '/agents/', activeMatch: '^/agents/' },
      { text: 'Reference', link: '/reference/configuration', activeMatch: '^/reference/' },
      { text: 'Project', link: '/project/contributing', activeMatch: '^/project/' },
      { text: 'Download', link: `${REPO}/releases` }
    ],
    sidebar: {
      '/guide/': [
        {
          text: 'Get started',
          items: [
            { text: 'Install', link: '/guide/install' },
            { text: 'First run and accounts', link: '/guide/first-run' },
            { text: 'Configuration overview', link: '/guide/configuration' }
          ]
        },
        {
          text: 'Using Siple',
          items: [
            { text: 'Making and taking calls', link: '/guide/calls' },
            { text: 'Transfers', link: '/guide/transfers' },
            { text: 'Extensions and BLF', link: '/guide/extensions' },
            { text: 'Keyboard shortcuts', link: '/guide/shortcuts' },
            { text: 'Tray and notifications', link: '/guide/tray' },
            { text: 'sip: and tel: links', link: '/guide/links' },
            { text: 'Network changes', link: '/guide/network' }
          ]
        },
        { text: 'Troubleshooting', link: '/guide/troubleshooting' }
      ],
      '/agents/': [
        {
          text: 'AI agents',
          items: [
            { text: 'Overview', link: '/agents/' },
            { text: 'Agent skill (CLI)', link: '/agents/skill' },
            { text: 'Quick start with MCP', link: '/agents/mcp-quickstart' },
            { text: 'JSON-RPC basics', link: '/agents/json-rpc' },
            { text: 'API reference', link: '/agents/api-reference' }
          ]
        }
      ],
      '/reference/': [
        {
          text: 'Reference',
          items: [
            { text: 'Configuration', link: '/reference/configuration' },
            { text: 'Control API', link: '/reference/api' },
            { text: 'Architecture', link: '/reference/architecture' }
          ]
        }
      ],
      '/project/': [
        {
          text: 'Project',
          items: [
            { text: 'Contributing', link: '/project/contributing' },
            { text: 'Security', link: '/project/security' },
            { text: 'Licence', link: '/project/licence' },
            { text: 'Changelog', link: '/project/changelog' }
          ]
        }
      ]
    },
    socialLinks: [{ icon: 'github', link: REPO }],
    search: { provider: 'local' },
    outline: { level: [2, 3], label: 'On this page' },
    editLink: {
      // Serialized into the client bundle: must not reference module-level values.
      pattern: ({ filePath }) => {
        const included: Record<string, string> = {
          'reference/configuration.md': 'docs/CONFIG.md',
          'agents/api-reference.md': 'docs/API.md',
          'reference/architecture.md': 'docs/ARCHITECTURE.md',
          'project/contributing.md': 'CONTRIBUTING.md',
          'project/security.md': 'SECURITY.md',
          'project/changelog.md': 'CHANGELOG.md'
        }
        return `https://github.com/zyno-io/siple/edit/main/${included[filePath] ?? 'website/' + filePath}`
      },
      text: 'Edit this page on GitHub'
    },
    footer: {
      message: 'Source-available under FSL-1.1-ALv2. Questions: oss@zyno.io',
      copyright: 'Copyright 2026 Signal24 LLC dba Zyno Consulting'
    }
  }
})
