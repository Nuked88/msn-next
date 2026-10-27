import assert from 'node:assert/strict'
import { test } from 'node:test'
import {
  formatBytes,
  messageParts,
  parseExtensions,
  prependOlderMessages,
  textIndexAtByteOffset,
} from '../src/lib/messages.ts'

test('le estensioni accettano virgole, spazi e punti iniziali', () => {
  assert.deepEqual(parseExtensions(' .JPG, png  mp4,,'), ['jpg', 'png', 'mp4'])
  assert.deepEqual(parseExtensions(''), [])
})

test('le dimensioni sono leggibili', () => {
  assert.equal(formatBytes(512), '512 B')
  assert.equal(formatBytes(1536), '1.5 KB')
  assert.equal(formatBytes(100 * 1024 * 1024), '100.0 MB')
  assert.equal(formatBytes(5 * 1024 ** 3), '5.00 GB')
})

test('gli offset in byte UTF-8 diventano indici JavaScript', () => {
  const text = 'è😀a'
  assert.equal(textIndexAtByteOffset(text, 0), 0)
  assert.equal(textIndexAtByteOffset(text, 2), 1)
  assert.equal(textIndexAtByteOffset(text, 6), 3)
})

test('le emoticon personalizzate cadono nel punto giusto dopo testo accentato', () => {
  const custom = { assetId: 'abc', name: 'gatto' }
  const body = 'perché :gatto: :)'
  const start = new TextEncoder().encode('perché ').length
  const end = start + ':gatto:'.length
  const parts = messageParts(body, [{ start, end, assetId: 'abc' }], [custom])
  assert.deepEqual(parts.map((part) => part.text), ['perché ', ':gatto:', ' ', ':)'])
  assert.equal(parts[1].custom, custom)
  assert.equal(parts[3].emoticon?.glyph, '🙂')
})

test('uno span sconosciuto resta testo e quelli sovrapposti vengono ignorati', () => {
  const parts = messageParts('ab:x:cd', [
    { start: 2, end: 5, assetId: 'manca' },
    { start: 3, end: 6, assetId: 'manca' },
  ], [])
  assert.deepEqual(parts.map((part) => part.text), ['ab', ':x:', 'cd'])
  assert.equal(parts[1].custom, undefined)
})

test('la paginazione non duplica i messaggi già caricati', () => {
  const current = [{ id: '3' }, { id: '4' }]
  const older = [{ id: '1' }, { id: '2' }, { id: '3' }]
  assert.deepEqual(prependOlderMessages(current, older).map((message) => message.id), ['1', '2', '3', '4'])
})
