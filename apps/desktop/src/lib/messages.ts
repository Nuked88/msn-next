// Logica dei messaggi senza dipendenze dalla UI, così si può testare da sola.

export type Emoticon = { glyph: string; shortcut: string; label: string }
export type EmoticonSpan = { start: number; end: number; assetId: string }
export type MessagePart<Custom> = { text: string; emoticon?: Emoticon; custom?: Custom }

export const BUILTIN_EMOTICONS: Emoticon[] = [
  { glyph: '🙂', shortcut: ':)', label: 'Sorriso' },
  { glyph: '😄', shortcut: ':D', label: 'Risata' },
  { glyph: '😉', shortcut: ';)', label: 'Occhiolino' },
  { glyph: '😛', shortcut: ':P', label: 'Linguaccia' },
  { glyph: '😢', shortcut: ':(', label: 'Triste' },
  { glyph: '😮', shortcut: ':o', label: 'Sorpresa' },
  { glyph: '❤️', shortcut: '<3', label: 'Cuore' },
  { glyph: '😎', shortcut: '8)', label: 'Forte' },
  { glyph: '😐', shortcut: ':|', label: 'Neutro' },
  { glyph: '😳', shortcut: ':$', label: 'Imbarazzato' },
  { glyph: '😘', shortcut: ':*', label: 'Bacio' },
  { glyph: '😠', shortcut: ':@', label: 'Arrabbiato' },
  { glyph: '😇', shortcut: 'O:)', label: 'Angelo' },
  { glyph: '😈', shortcut: '}:)', label: 'Diavoletto' },
  { glyph: '😴', shortcut: '|-)', label: 'Assonnato' },
  { glyph: '👍', shortcut: '(y)', label: 'Pollice su' },
  { glyph: '👎', shortcut: '(n)', label: 'Pollice giù' },
  { glyph: '🌹', shortcut: '@}-', label: 'Rosa' },
  { glyph: '🎉', shortcut: '\\o/', label: 'Festa' },
]

/** Estensioni per l'auto-accept: separate da virgole o spazi, senza punto. */
export function parseExtensions(value: string): string[] {
  return value.split(/[\s,]+/).map((ext) => ext.trim().replace(/^\./, '').toLowerCase()).filter(Boolean)
}

export function formatBytes(bytes: number) {
  if (bytes < 1024) return `${bytes} B`
  if (bytes < 1024 ** 2) return `${(bytes / 1024).toFixed(1)} KB`
  if (bytes < 1024 ** 3) return `${(bytes / 1024 ** 2).toFixed(1)} MB`
  return `${(bytes / 1024 ** 3).toFixed(2)} GB`
}

/** Il core indica le emoticon in byte UTF-8; JavaScript conta in unità UTF-16. */
export function textIndexAtByteOffset(text: string, offset: number) {
  let bytes = 0
  let index = 0
  for (const character of text) {
    if (bytes >= offset) break
    bytes += new TextEncoder().encode(character).length
    index += character.length
  }
  return index
}

export function builtinMessageParts<Custom>(text: string): MessagePart<Custom>[] {
  const escaped = BUILTIN_EMOTICONS
    .map((item) => item.shortcut.replace(/[.*+?^${}()|[\]\\]/g, '\\$&'))
    .join('|')
  const matcher = new RegExp(`(${escaped})`, 'g')
  return text.split(matcher).filter(Boolean).map((part) => ({
    text: part,
    emoticon: BUILTIN_EMOTICONS.find((item) => item.shortcut === part),
  }))
}

/** Divide il testo in parti: testo, emoticon predefinite e personalizzate. */
export function messageParts<Custom extends { assetId: string }>(
  body: string,
  spans: EmoticonSpan[],
  available: Custom[],
): MessagePart<Custom>[] {
  if (!spans.length) return builtinMessageParts(body)
  const parts: MessagePart<Custom>[] = []
  let cursor = 0
  for (const span of [...spans].sort((a, b) => a.start - b.start)) {
    const start = textIndexAtByteOffset(body, span.start)
    const end = textIndexAtByteOffset(body, span.end)
    if (start < cursor || end <= start) continue
    parts.push(...builtinMessageParts<Custom>(body.slice(cursor, start)))
    const custom = available.find((item) => item.assetId === span.assetId)
    parts.push(custom
      ? { text: body.slice(start, end), custom }
      : { text: body.slice(start, end) })
    cursor = end
  }
  parts.push(...builtinMessageParts<Custom>(body.slice(cursor)))
  return parts
}

/** Antepone una pagina di messaggi più vecchi, senza duplicare quelli già presenti. */
export function prependOlderMessages<Message extends { id: string }>(current: Message[], older: Message[]) {
  const known = new Set(current.map((message) => message.id))
  return [...older.filter((message) => !known.has(message.id)), ...current]
}
