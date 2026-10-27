import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import { test } from 'node:test'

const read = (path: string) => readFileSync(new URL(path, import.meta.url), 'utf8')
const en: Record<string, string> = JSON.parse(read('../src/locales/en.json'))
const it: Record<string, string> = JSON.parse(read('../src/locales/it.json'))
const placeholders = (text: string) => [...text.matchAll(/\{(\w+)\}/g)].map((match) => match[1]).sort()

test('italiano e inglese hanno le stesse chiavi', () => {
  assert.deepEqual(Object.keys(it).sort(), Object.keys(en).sort())
})

test('le traduzioni usano gli stessi segnaposto', () => {
  for (const key of Object.keys(en)) {
    assert.deepEqual(placeholders(it[key] ?? ''), placeholders(en[key]), key)
  }
})

test('ogni chiave usata dalla UI esiste', () => {
  const source = read('../src/AppMsn.svelte')
  const used = new Set([...source.matchAll(/\$t\('([^']+)'/g)].map((match) => match[1]))
  const missing = [...used].filter((key) => !(key in en))
  assert.deepEqual(missing, [])
})
