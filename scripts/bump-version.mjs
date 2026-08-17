#!/usr/bin/env node
// Prompt-driven version bump for apps/desktop/package.json (tauri.conf reads it).
// Usage: node scripts/bump-version.mjs [patch|minor|major|keep]
//   - with arg: non-interactive
//   - no arg + TTY: prompts
//   - no arg + no TTY (CI): keeps current version
import { readFileSync, writeFileSync } from 'node:fs'
import { fileURLToPath } from 'node:url'
import { dirname, join } from 'node:path'
import { createInterface } from 'node:readline'

const pkgPath = join(dirname(fileURLToPath(import.meta.url)), '..', 'apps', 'desktop', 'package.json')
const pkg = JSON.parse(readFileSync(pkgPath, 'utf8'))
const [major, minor, patch] = pkg.version.split('.').map(Number)

function bump(kind) {
  switch (kind) {
    case 'major': return `${major + 1}.0.0`
    case 'minor': return `${major}.${minor + 1}.0`
    case 'patch': return `${major}.${minor}.${patch + 1}`
    case 'keep': return pkg.version
    default: return null
  }
}

function apply(kind) {
  const next = bump(kind)
  if (next === null) { console.error(`Segmento sconosciuto: ${kind}`); process.exit(2) }
  if (next !== pkg.version) {
    pkg.version = next
    writeFileSync(pkgPath, JSON.stringify(pkg, null, 2) + '\n')
    console.log(`Versione: ${next}`)
  } else {
    console.log(`Versione invariata: ${next}`)
  }
}

const arg = process.argv[2]
if (arg) { apply(arg); process.exit(0) }
if (!process.stdin.isTTY) { console.log(`Versione invariata: ${pkg.version} (non-interattivo)`); process.exit(0) }

const rl = createInterface({ input: process.stdin, output: process.stdout })
rl.question(
  `Versione attuale ${pkg.version}. Incremento? [patch/minor/major/keep] (default keep): `,
  (answer) => { rl.close(); apply((answer.trim() || 'keep').toLowerCase()) },
)
