import { defineConfig } from 'vitepress'

const site = 'https://briansunter.github.io/bevy-agent/'

export default defineConfig({
  title: 'Bevy Agent',
  description: 'Build a Bevy simulation you can step, inspect, snapshot, and replay. Clear guides for Rust, CLI, and Python clients.',
  base: '/bevy-agent/',
  lang: 'en-US',
  srcExclude: ['README.md', 'public/**', 'reference/architecture-review.md'],
  lastUpdated: true,
  cleanUrls: false,
  sitemap: { hostname: site },
  head: [
    ['link', { rel: 'icon', type: 'image/svg+xml', href: '/bevy-agent/logo.svg' }],
    ['meta', { name: 'theme-color', content: '#4158c5' }],
    ['meta', { property: 'og:type', content: 'website' }],
    ['meta', { property: 'og:site_name', content: 'Bevy Agent' }],
  ],
  markdown: { lineNumbers: true },
  themeConfig: {
    logo: '/logo.svg',
    siteTitle: 'Bevy Agent',
    nav: [
      { text: 'Guide', link: '/getting-started', activeMatch: '/(getting-started|concepts|guides|controllable-game)' },
      { text: 'Reference', link: '/reference/crates', activeMatch: '/(reference|architecture)' },
      { text: '0.0.4', items: [
        { text: 'Release notes', link: '/reference/releases' },
        { text: 'Crates on crates.io', link: 'https://crates.io/users/briansunter' },
      ] },
    ],
    sidebar: [
      { text: 'Start here', items: [
        { text: 'Getting started', link: '/getting-started' },
        { text: 'Runnable examples', link: '/examples' },
        { text: 'How the control loop works', link: '/concepts' },
        { text: 'Choose your crates', link: '/reference/crates' },
      ] },
      { text: 'Build your environment', items: [
        { text: 'Integrate a Bevy game', link: '/controllable-game' },
        { text: 'Actions and observations', link: '/guides/actions-observations' },
        { text: 'Snapshots and replay', link: '/guides/snapshots-replay' },
        { text: 'Testing and reproducibility', link: '/guides/testing' },
        { text: 'Screenshots and rendering', link: '/guides/capture' },
      ] },
      { text: 'Connect a client', items: [
        { text: 'HTTP and the CLI', link: '/guides/remote-control' },
        { text: 'Python', link: '/guides/python' },
        { text: 'WebSocket and stdio', link: '/guides/transports' },
        { text: 'Retries and recovery', link: '/guides/recovery' },
      ] },
      { text: 'Reference', items: [
        { text: 'JSON-RPC methods', link: '/reference/protocol' },
        { text: 'Architecture', link: '/architecture' },
        { text: 'Troubleshooting', link: '/reference/troubleshooting' },
        { text: 'Release notes', link: '/reference/releases' },
        { text: 'Contributing and docs', link: '/reference/contributing' },
        { text: 'Publishing', link: '/publishing' },
      ] },
    ],
    search: { provider: 'local', options: { detailedView: true } },
    outline: { level: [2, 3], label: 'On this page' },
    editLink: { pattern: 'https://github.com/briansunter/bevy-agent/edit/master/docs/:path', text: 'Improve this page on GitHub' },
    footer: { message: 'Released under MIT or Apache-2.0.', copyright: 'Bevy Agent · Built for controlled simulations' },
    socialLinks: [{ icon: 'github', link: 'https://github.com/briansunter/bevy-agent' }],
    docFooter: { prev: 'Previous guide', next: 'Next guide' },
  },
})
