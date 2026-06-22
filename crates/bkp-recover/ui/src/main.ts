// Copyright (c) 2026 Nyx Software, LLC
// SPDX-License-Identifier: Apache-2.0
// Nyx Backup Recovery - https://nyxbackup.com

import './app.css'
import { mount } from 'svelte'
import App from './App.svelte'

// Stamp the host platform on <html> before first paint so platform-
// specific CSS / Svelte branches (macOS traffic-lights gutter, native
// NSMenu wired in src/bin/gui.rs) take effect on the very first render
// with no flash of wrong layout.  Mirrors crates/bkp-gui/ui/src/main.ts.
{
  const p = navigator.platform || ''
  const platform = p.startsWith('Mac')   ? 'mac'
                 : p.startsWith('Win')   ? 'win'
                 : p.startsWith('Linux') ? 'linux'
                 : 'unknown'
  document.documentElement.setAttribute('data-platform', platform)
}

// macOS-only: disable WKWebView's auto-capitalise + autocorrect +
// spellcheck on every text-style <input>.  WKWebView honours the macOS
// system "Capitalize words automatically" preference by default, which
// feels right for messaging apps but wrong here - filenames, storage
// URLs, OAuth client IDs all want raw user input.  Windows + Linux
// Tauri builds don't have this behaviour so leaving their attributes
// alone keeps the input fields' platform-native semantics intact.
//
// MutationObserver attaches the attributes to nodes inserted at any
// point in the app's lifetime, so Svelte components mounted after
// this initial pass (Connect.svelte's storage URL field, Settings,
// etc.) get the treatment too.
if (document.documentElement.getAttribute('data-platform') === 'mac') {
  const TEXT_INPUT_SELECTOR =
    'input[type="text"], input[type="email"], input[type="url"], ' +
    'input[type="search"], input[type="tel"], input:not([type]), textarea'

  const applyTo = (root: ParentNode) => {
    root.querySelectorAll<HTMLInputElement>(TEXT_INPUT_SELECTOR).forEach(el => {
      if (!el.hasAttribute('autocapitalize')) el.setAttribute('autocapitalize', 'off')
      if (!el.hasAttribute('autocorrect'))    el.setAttribute('autocorrect',    'off')
      if (!el.hasAttribute('spellcheck'))     el.setAttribute('spellcheck',     'false')
    })
  }
  applyTo(document)
  new MutationObserver(records => {
    records.forEach(rec => {
      rec.addedNodes.forEach(n => {
        if (n.nodeType === 1) applyTo(n as Element)
      })
    })
  }).observe(document.body, { childList: true, subtree: true })
}

// Apply the persisted theme before mounting.  The Recovery Tool reads its
// own settings file (NOT localStorage like the main app), so a fresh run
// after install picks up whatever theme the user last chose.
async function applySettings() {
  try {
    const { invoke } = await import('@tauri-apps/api/core')
    const s = await invoke<{ theme?: string; locale?: string }>('rec_get_settings')
    if (s?.theme) document.documentElement.dataset.theme = s.theme
    if (s?.locale && s.locale !== 'auto') {
      const { setLocaleOverride } = await import('./lib/i18n.svelte.ts')
      await setLocaleOverride(s.locale)
    }
  } catch {
    // Falls back to the :root default theme and OS-locale i18n.
  }
}
applySettings()

const app = mount(App, { target: document.getElementById('app')! })
export default app
