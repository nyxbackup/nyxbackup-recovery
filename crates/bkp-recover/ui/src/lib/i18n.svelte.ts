// Copyright (c) 2026 Nyx Software, LLC
// SPDX-License-Identifier: Apache-2.0
// Nyx Backup Recovery - https://nyxbackup.com

// i18n for the Recovery Tool.  Resolves the locale from the saved
// settings override, else navigator.language, falling back to English.
// The Settings screen exposes an in-app language picker via
// setLocaleOverride().  Locale files live in repo-root `locales/*.json`
// and are embedded in the webview bundle: English is imported statically
// and the other 23 languages lazy-load on demand.  New `gui.recover.*`
// keys go to en.json first, then Google Translate + Opus review for the
// non-EN locales (see docs/TRANSLATION_WORKFLOW.md).

import en from '@locales/en.json'

// Static import of the English locale - guaranteed shipped.  Non-EN
// locales lazy-load if the system language matches a supported one.
const LOCALES: Record<string, Record<string, string>> = {
  en: en as Record<string, string>,
}

const SUPPORTED = [
  'en', 'cs', 'da', 'de', 'el', 'es', 'fi', 'fr', 'hi', 'hu', 'it',
  'ja', 'ko', 'nb', 'nl', 'pl', 'pt', 'ro', 'ru', 'sv', 'tr', 'uk',
  'vi', 'zh',
]

function pickLocale(override?: string): string {
  // Explicit override from settings.json wins.  'auto' or missing falls
  // through to navigator.language, which falls through to English.
  if (override && override !== 'auto' && SUPPORTED.includes(override)) {
    return override
  }
  const raw = (navigator?.language || 'en').toLowerCase()
  if (SUPPORTED.includes(raw)) return raw
  const primary = raw.split('-')[0]
  if (SUPPORTED.includes(primary)) return primary
  return 'en'
}

const locale = $state({ id: pickLocale() })
// Reactive: `t()` reads this, so reassigning it (on locale change / lazy-load)
// re-renders every component that called t() - live language switching.
let activeMap: Record<string, string> = $state(LOCALES.en)

// Lazy-load the non-EN map if needed.  Failures fall back to English
// silently - the Recovery Tool is the wrong moment to surface a
// localisation error.
async function loadNonEn(id: string) {
  if (LOCALES[id]) {
    activeMap = LOCALES[id]
    return
  }
  try {
    const mod = await import(`@locales/${id}.json`)
    LOCALES[id] = mod.default as Record<string, string>
    activeMap = LOCALES[id]
  } catch {
    activeMap = LOCALES.en
  }
}
if (locale.id !== 'en') loadNonEn(locale.id)

/// Apply a manual locale override from the Settings screen.  Called by
/// `main.ts` at startup after reading the saved settings.json, and by
/// the Settings page after Save.
export async function setLocaleOverride(override: string) {
  const next = pickLocale(override)
  if (next === locale.id) return
  locale.id = next
  if (next === 'en') {
    activeMap = LOCALES.en
  } else {
    await loadNonEn(next)
  }
}

/// Look up a key.  Returns the English value as a last-resort fallback so
/// the UI never shows a bare key.
export function t(key: string): string {
  return activeMap[key] ?? LOCALES.en[key] ?? key
}

/// Look up a key with `{placeholder}` substitution.  Trailing or missing
/// placeholders are left intact so the UI doesn't render `undefined`.
export function tf(key: string, vars: Record<string, string | number>): string {
  const template = t(key)
  return template.replace(/\{(\w+)\}/g, (_, name) =>
    Object.hasOwn(vars, name) ? String(vars[name]) : `{${name}}`)
}

/// Current active locale ID.  Exported for debugging / About-screen display.
export function currentLocale(): string { return locale.id }

/// Format a timestamp (ms since epoch) in the active UI locale, so dates/times
/// follow the user's language (e.g. Polish `dd.MM.yyyy`, 24-hour) instead of
/// the webview's default en-US.  Reads the reactive locale, so switching
/// language live re-formats.  `opts` are standard `Intl.DateTimeFormatOptions`.
export function fmtDateTime(ms: number, opts?: Intl.DateTimeFormatOptions): string {
  const tag = locale.id && locale.id !== 'auto' ? locale.id : undefined
  return new Date(ms).toLocaleString(tag, opts)
}
