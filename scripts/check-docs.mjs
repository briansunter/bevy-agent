import { readdirSync, readFileSync, existsSync } from 'node:fs'
import { join, relative } from 'node:path'

const root = 'docs/.vitepress/dist'
const base = '/bevy-agent/'
const pages = []
function walk(dir) {
  for (const entry of readdirSync(dir, { withFileTypes: true })) {
    const path = join(dir, entry.name)
    if (entry.isDirectory()) walk(path)
    else if (entry.name.endsWith('.html')) pages.push(path)
  }
}
walk(root)
const failures = []
let checked = 0
for (const page of pages) {
  const html = readFileSync(page, 'utf8')
  const current = new URL(base + relative(root, page), 'https://docs.local')
  for (const match of html.matchAll(/(?:href|src)="([^"<>]+)"/g)) {
    const value = match[1].replaceAll('&amp;', '&')
    if (/^(?:https?:|data:|mailto:|obsidian:|javascript:)/.test(value)) continue
    const url = new URL(value, current)
    if (url.origin !== current.origin) continue
    if (!url.pathname.startsWith(base)) { failures.push(page + ': outside base: ' + value); continue }
    let path = join(root, decodeURIComponent(url.pathname.slice(base.length)))
    if (url.pathname.endsWith('/')) path = join(path, 'index.html')
    else if (!existsSync(path) && !url.pathname.split('/').at(-1).includes('.')) path += '.html'
    if (!existsSync(path)) { failures.push(page + ': missing: ' + value); continue }
    if (url.hash && path.endsWith('.html')) {
      const target = readFileSync(path, 'utf8')
      const id = decodeURIComponent(url.hash.slice(1))
      if (!target.includes('id="' + id + '"')) failures.push(page + ': missing anchor: ' + value)
    }
    checked++
  }
}
for (const route of ['getting-started.html', 'reference/crates.html', 'guides/remote-control.html', 'reference/protocol.html']) {
  if (!existsSync(join(root, route))) failures.push('Required route missing: ' + route)
}
const first = readFileSync(join(root, 'getting-started.html'), 'utf8')
if (!first.includes('Snapshot restore and replay matched')) failures.push('Counter expected output missing')
if (!first.includes('register_required_snapshot_resource')) failures.push('Complete counter source missing')
const crates = readFileSync(join(root, 'reference/crates.html'), 'utf8')
for (const name of ['core', 'snapshot', 'replay', 'runner', 'remote', 'cli']) {
  if (!crates.includes('https://crates.io/crates/bevy_agent_' + name)) failures.push('Crate link missing: ' + name)
}
if (failures.length) { console.error(failures.join('\n')); process.exit(1) }
console.log('Verified ' + pages.length + ' pages and ' + checked + ' local links/assets, including code, anchors, and all six crates.')
