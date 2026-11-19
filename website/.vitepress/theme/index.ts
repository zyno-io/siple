import DefaultTheme from 'vitepress/theme'
import type { Theme } from 'vitepress'
import '@fontsource/inter/400.css'
import '@fontsource/inter/500.css'
import '@fontsource/inter/600.css'
import '@fontsource/inter/700.css'
import Keys from './components/Keys.vue'
import OsTabs from './components/OsTabs.vue'
import Shot from './components/Shot.vue'
import StartPaths from './components/StartPaths.vue'
import './custom.css'

export default {
  extends: DefaultTheme,
  enhanceApp({ app }) {
    app.component('Keys', Keys)
    app.component('OsTabs', OsTabs)
    app.component('Shot', Shot)
    app.component('StartPaths', StartPaths)
  }
} satisfies Theme
