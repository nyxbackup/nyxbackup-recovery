// Copyright (c) 2026 Nyx Software, LLC
// SPDX-License-Identifier: Apache-2.0
// Nyx Backup Recovery - https://nyxbackup.com

/**
 * Turn a rejected Tauri command error into a plain-language, localized message.
 *
 * The Rust side (`crate::errors::user_error`) packages user-facing failures as
 *   `<code>` U+0001 `<english message>` U+0001 `<raw detail>`
 * where `<code>` is a stable `err.*` i18n key.  This splits that apart,
 * translates the code through the active locale (falling back to the bundled
 * English message when a locale lacks the key), and returns the plain sentence.
 * The raw technical detail is deliberately NOT shown - it lives in
 * `recovery.log` for diagnosis.
 *
 * Legacy / unclassified errors (no U+0001 separator, e.g. an OAuth message or a
 * JS exception) are returned as-is, so this is safe to use everywhere
 * `String(e)` was used before.
 */
import { t } from './i18n.svelte'

// U+0001 (SOH) - the field separator the Rust side uses; never appears in a
// real message.
const SEP = '\u0001'

export function friendlyError(e: unknown): string {
  const raw =
    typeof e === 'string' ? e : e instanceof Error ? e.message : String(e)

  if (!raw.includes(SEP)) return raw

  const [code, fallback = ''] = raw.split(SEP)
  if (!code.startsWith('err.')) return raw

  const translated = t(code)
  // t() returns the key itself when no locale (incl. English) defines it.
  return translated && translated !== code ? translated : fallback
}
