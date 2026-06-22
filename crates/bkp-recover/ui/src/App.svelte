<!--
  Copyright (c) 2026 Nyx Software, LLC
  SPDX-License-Identifier: Apache-2.0
  Nyx Backup Recovery - https://nyxbackup.com

  Top-level shell for the Recovery Tool.  Routes between five linear
  screens based on a single `phase` state.  Custom title bar (no native
  decorations on any platform).
-->
<script lang="ts">
  import { onMount } from 'svelte'
  import { getCurrentWindow } from '@tauri-apps/api/window'
  import { listen } from '@tauri-apps/api/event'
  import Connect from './views/Connect.svelte'
  import Unlock from './views/Unlock.svelte'
  import Browse from './views/Browse.svelte'
  import About from './views/About.svelte'
  import Settings from './views/Settings.svelte'
  import logoUrl from './lib/logo.png'
  import { t } from './lib/i18n.svelte.ts'

  type Phase = 'connect' | 'unlock' | 'browse' | 'restoring' | 'done'

  let phase = $state<Phase>('connect')
  let showAbout = $state(false)
  let showSettings = $state(false)
  let isMaximised = $state(false)

  const appWindow = getCurrentWindow()

  // Platform detection - stamped on <html> by main.ts before first paint
  // (same pattern as the main GUI in TitleBar.svelte).  On macOS the
  // Tauri window is built with decorations=true + titleBarStyle="Visible"
  // via tauri.macos.conf.json, the OS draws traffic lights upper-left,
  // and Settings / About are also wired into the native NSMenu in
  // src/bin/gui.rs (About / Preferences ⌘,).  The full Win + Linux
  // title-bar strip (⚙ + ? + min / max / close) is suppressed on macOS;
  // in its place a lightweight top-right overlay carries just the ⚙ + ?
  // buttons (min / max / close are handled by the native traffic lights).
  const isMac = typeof document !== 'undefined'
    && document.documentElement.getAttribute('data-platform') === 'mac'

  // Lock down browser-style shortcuts that would let the user navigate
  // the webview around (Ctrl-R / F5 reload, Ctrl-P print, Ctrl-F find,
  // F12 devtools, Ctrl-+ / Ctrl-- / Ctrl-0 zoom).  Clipboard shortcuts
  // (Ctrl-C / Ctrl-X / Ctrl-V / Ctrl-A) are deliberately preserved so
  // copy-paste in input fields still works.  Same code as the main app.
  function suppressNavShortcuts(e: KeyboardEvent) {
    const k = e.key
    const lk = k.toLowerCase()
    const ctrl = e.ctrlKey || e.metaKey
    if (k === 'F3' || k === 'F5' || k === 'F7' || k === 'F11' || k === 'F12') {
      e.preventDefault(); e.stopPropagation(); return
    }
    if (!ctrl) return
    if (lk === 'c' || lk === 'x' || lk === 'v' || lk === 'a') return
    const blocked = ['r', 'p', 'f', 'g', 'h', 'j', 'l', 's', 'u', 'o', 'w', 't', 'n']
    if (blocked.includes(lk)) {
      e.preventDefault(); e.stopPropagation(); return
    }
    if (k === '+' || k === '=' || k === '-' || k === '0') {
      e.preventDefault(); e.stopPropagation(); return
    }
  }
  function suppressContextMenu(e: MouseEvent) { e.preventDefault() }

  onMount(async () => {
    window.addEventListener('keydown', suppressNavShortcuts, /*capture=*/ true)
    window.addEventListener('contextmenu', suppressContextMenu, /*capture=*/ true)
    isMaximised = await appWindow.isMaximized()
    const unlistenResize = await appWindow.onResized(async () => {
      isMaximised = await appWindow.isMaximized()
    })
    // macOS app menu wires About / Preferences into the native NSMenu
    // (src/bin/gui.rs).  Selecting them fires Tauri events the renderer
    // listens for here to open the same modals the in-window ⚙ + ?
    // buttons drive on Windows + Linux.
    const unlistenSettings = await listen('menu://show-settings',
      () => { showSettings = true })
    const unlistenAbout    = await listen('menu://show-about',
      () => { showAbout = true })
    return () => {
      unlistenResize()
      unlistenSettings()
      unlistenAbout()
    }
  })

  function handlePhase(next: Phase) { phase = next }
</script>

<!-- titleBarStyle "Visible" (Mac default) means the OS-drawn title bar
     occupies its own space above the content view; no manual top
     padding needed. -->
<div class="relative flex flex-col h-screen bg-nyx-bg text-nyx-text">
  <!-- Title bar.  Per-platform shape (newer):
       - macOS: SUPPRESSED entirely.  The OS-drawn title bar (with
                native traffic lights and the app-menu title) is the
                only chrome a Mac app should carry; the wordmark
                lives in the macOS app menu's first item ("Nyx
                Backup Recovery"), Settings + About live in that
                same menu (Preferences ⌘, / About Nyx Backup
                Recovery).
       - Win + Linux: 36 px custom strip with logo + wordmark +
                Settings / About / min / max / close buttons. -->
  {#if !isMac}
  <div
    data-tauri-drag-region
    class="flex items-center justify-between h-9 px-3 bg-nyx-surface border-b border-nyx-border select-none"
  >
    <div class="flex items-center gap-2 pointer-events-none">
      <img src={logoUrl} alt="Nyx Backup" class="w-5 h-5 rounded-sm" />
      <span class="text-xs font-semibold">Nyx Backup Recovery</span>
      <span class="text-[10px] text-nyx-muted">FREE</span>
    </div>
    <div class="flex items-center gap-1">
      <button
        onclick={() => (showSettings = true)}
        title={t('gui.recover.chrome.settings')}
        class="px-2 py-1 text-xs text-nyx-muted hover:text-nyx-text hover:bg-nyx-surface2 rounded transition-colors"
      >⚙</button>
      <button
        onclick={() => (showAbout = true)}
        title={t('gui.recover.chrome.about')}
        class="px-2 py-1 text-xs text-nyx-muted hover:text-nyx-text hover:bg-nyx-surface2 rounded transition-colors"
      >?</button>
      <button
        onclick={async () => appWindow.minimize()}
        title={t('gui.recover.chrome.minimize')}
        class="px-2 py-1 text-xs text-nyx-muted hover:text-nyx-text hover:bg-nyx-surface2 rounded transition-colors"
      >&minus;</button>
      <button
        onclick={async () => isMaximised ? appWindow.unmaximize() : appWindow.maximize()}
        title={isMaximised ? t('gui.recover.chrome.unmaximize') : t('gui.recover.chrome.maximize')}
        class="px-2 py-1 text-xs text-nyx-muted hover:text-nyx-text hover:bg-nyx-surface2 rounded transition-colors"
      >{isMaximised ? '⧉' : '☐'}</button>
      <button
        onclick={async () => appWindow.close()}
        title={t('gui.action.close')}
        class="px-2 py-1 text-xs text-nyx-muted hover:text-white hover:bg-nyx-error rounded transition-colors"
      >×</button>
    </div>
  </div>
  {/if}

  <!-- macOS: the in-window title-bar buttons above are suppressed (the OS
       draws the traffic lights + native menu bar).  About / Preferences
       do live in the native menu bar, but that isn't discoverable enough,
       so we add a small floating overlay with the same ⚙ + ? buttons.
       Placed top-RIGHT to clear the native traffic lights (top-left), and
       hidden while a modal is open so it never overlaps the modal chrome.
       Win + Linux are unaffected - they use the {#if !isMac} strip above. -->
  {#if isMac && !showSettings && !showAbout}
  <div class="absolute top-2 right-3 z-50 flex items-center gap-1">
    <button
      onclick={() => (showSettings = true)}
      title={t('gui.recover.chrome.settings')}
      aria-label={t('gui.recover.chrome.settings')}
      class="px-2 py-1 text-sm text-nyx-muted hover:text-nyx-text hover:bg-nyx-surface2 rounded transition-colors"
    >⚙</button>
    <button
      onclick={() => (showAbout = true)}
      title={t('gui.recover.chrome.about')}
      aria-label={t('gui.recover.chrome.about')}
      class="px-2 py-1 text-sm text-nyx-muted hover:text-nyx-text hover:bg-nyx-surface2 rounded transition-colors"
    >?</button>
  </div>
  {/if}

  <!-- Main content. -->
  <div class="flex-1 overflow-auto">
    {#if showSettings}
      <Settings onClose={() => (showSettings = false)} />
    {:else if showAbout}
      <About onClose={() => (showAbout = false)} />
    {:else if phase === 'connect'}
      <!-- Connect.svelte does both Connect AND Unlock in one form, so
           jump straight to 'browse'.  The standalone Unlock screen
           below is reserved for the checkpoint-resume path where the
           user is asked for the key alone. -->
      <Connect onConnected={() => handlePhase('browse')} />
    {:else if phase === 'unlock'}
      <Unlock onUnlocked={() => handlePhase('browse')} onBack={() => handlePhase('connect')} />
    {:else if phase === 'browse'}
      <Browse onBack={() => handlePhase('connect')} />
    {/if}
  </div>
</div>
