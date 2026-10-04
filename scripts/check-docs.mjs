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
for (const route of ['getting-started.html', 'examples.html', 'guides/testing.html', 'reference/crates.html', 'guides/remote-control.html', 'reference/protocol.html']) {
  if (!existsSync(join(root, route))) failures.push('Required route missing: ' + route)
}
const first = readFileSync(join(root, 'getting-started.html'), 'utf8')
if (!first.includes('Snapshot restore and replay matched')) failures.push('Counter expected output missing')
if (!first.includes('register_required_snapshot_resource')) failures.push('Complete counter source missing')
const crates = readFileSync(join(root, 'reference/crates.html'), 'utf8')
for (const name of ['core', 'snapshot', 'replay', 'runner', 'remote', 'cli']) {
  if (!crates.includes('https://crates.io/crates/bevy_agent_' + name)) failures.push('Crate link missing: ' + name)
}
const version = readFileSync('Cargo.toml', 'utf8').match(/^version = "([^"]+)"$/m)?.[1]
if (!version) failures.push('Workspace release version missing')
if (!readFileSync('docs/getting-started.md', 'utf8').includes('experimental release ' + version + '.')) failures.push('Getting-started release description out of date')
const sources = ['README.md', 'docs/getting-started.md', 'docs/guides/remote-control.md',
  ...['core', 'snapshot', 'replay', 'runner', 'remote'].map(name => 'crates/bevy_agent_' + name + '/README.md'),
  'crates/agentctl/README.md']
for (const path of sources) {
  const text = readFileSync(path, 'utf8')
  for (const match of text.matchAll(/bevy_agent_\w+\s*=\s*"=([^"]+)"/g)) {
    if (match[1] !== version) failures.push(path + ': companion version differs from workspace: ' + match[1])
  }
  if (/127\.0\.0\.(?!1\b)\d+/.test(text)) failures.push(path + ': loopback examples must use 127.0.0.1')
  for (const match of text.matchAll(/cargo install bevy_agent_cli --version ([\d.]+)/g)) {
    if (match[1] !== version) failures.push(path + ': CLI installation version differs from workspace')
  }
}
for (const path of ['crates/agentctl/README.md', 'docs/guides/remote-control.md']) {
  if (!readFileSync(path, 'utf8').includes('http://127.0.0.1:4000/rpc')) failures.push(path + ': CLI default endpoint missing')
}
for (const name of ['core', 'snapshot', 'replay', 'runner', 'remote', 'cli']) {
  if (!crates.includes('https://crates.io/crates/bevy_agent_' + name + '/' + version)) failures.push('Crate release link out of date: ' + name)
}
if (failures.length) { console.error(failures.join('\n')); process.exit(1) }
console.log('Verified ' + pages.length + ' pages and ' + checked + ' local links/assets, including code, anchors, all six crates, release versions, and CLI endpoints.')
