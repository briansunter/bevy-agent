import DefaultTheme from 'vitepress/theme'
import '@fontsource-variable/manrope'
import '@fontsource/ibm-plex-mono/400.css'
import './style.css'
import DocsHome from './DocsHome.vue'
import CounterDemo from './CounterDemo.vue'

export default {
  extends: DefaultTheme,
  enhanceApp({ app }) {
    app.component('DocsHome', DocsHome)
    app.component('CounterDemo', CounterDemo)
  },
}
