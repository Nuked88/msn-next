// Minimal i18n. Add a language = add src/locales/<code>.json + one line in `available`.
// Usage in Svelte: import { t } from './lib/i18n'; then {$t('key')} or $t('key', { name })
import { writable, derived } from 'svelte/store'
import en from '../locales/en.json'
import it from '../locales/it.json'

const dictionaries: Record<string, Record<string, string>> = { en, it }

export const available = [
  { code: 'it', label: 'Italiano' },
  { code: 'en', label: 'English' },
]

const storageKey = 'msnnext-locale-v1'

function initialLocale(): string {
  if (typeof localStorage !== 'undefined') {
    const saved = localStorage.getItem(storageKey)
    if (saved && saved in dictionaries) return saved
  }
  const nav = typeof navigator !== 'undefined' ? navigator.language : 'en'
  const short = nav.slice(0, 2)
  return short in dictionaries ? short : 'en'
}

export const locale = writable<string>(initialLocale())
locale.subscribe((value) => {
  if (typeof localStorage !== 'undefined') localStorage.setItem(storageKey, value)
})

// Falls back to English, then the raw key, so a missing translation is visible but non-fatal.
export const t = derived(locale, ($locale) => (key: string, vars?: Record<string, string | number>) => {
  let text = dictionaries[$locale]?.[key] ?? en[key as keyof typeof en] ?? key
  if (vars) for (const [name, value] of Object.entries(vars)) {
    text = text.replaceAll(`{${name}}`, String(value))
  }
  return text
})
