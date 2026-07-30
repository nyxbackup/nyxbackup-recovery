<!--
  Copyright (c) 2026 Nyx Software, LLC
  SPDX-License-Identifier: Apache-2.0
  Nyx Backup Recovery - https://nyxbackup.com

  Settings dialog: download bandwidth, log level, theme.
  Persisted to ~/.local/share/nyxbackup-recover/settings.json via the
  rec_save_settings command.  Theme change applies immediately to
  document.documentElement.dataset.theme so the user sees it before
  saving.
-->
<script lang="ts">
  import { onMount } from 'svelte'
  import { api, type Settings } from '../lib/api'
  import { t, setLocaleOverride } from '../lib/i18n.svelte.ts'

  let { onClose } = $props<{ onClose: () => void }>()

  // Same set the main app exposes.  Each id must match a palette in
  // app.css: 'dark' is the :root default (no [data-theme] block), the
  // rest are [data-theme="<id>"] blocks.  'catppuccin' (NOT
  // 'catppuccin-mocha') is the Mocha palette's id in app.css.
  const THEMES = [
    { id: 'dark',           label: 'Dark' },
    { id: 'light',          label: 'Light' },
    { id: 'dracula',        label: 'Dracula' },
    { id: 'enchant',        label: 'Enchant' },
    { id: 'nord',           label: 'Nord' },
    { id: 'catppuccin',     label: 'Catppuccin Mocha' },
    { id: 'solarized-dark', label: 'Solarized Dark' },
    { id: 'solarized-light', label: 'Solarized Light' },
    { id: 'cyber',          label: 'Cyber' },
  ]

  const LOG_LEVELS = ['error', 'warn', 'info', 'debug', 'trace']
  // Same Mbps presets the main app uses (crates/bkp-gui/.../Settings.svelte).
  const BW_PRESETS = [10, 20, 50, 100, 200, 350, 500]

  // Language picker: 'auto' follows the OS locale; otherwise the 24
  // languages the main app supports.  Endonyms (native names) so users
  // can find their language without speaking English first.  Mirrors
  // the main app's language picker in Settings.
  const LANGUAGES: { id: string; label: string }[] = [
    { id: 'auto', label: 'Auto (follow OS language)' },
    { id: 'en',   label: 'English' },
    { id: 'es',   label: 'Español' },
    { id: 'fr',   label: 'Français' },
    { id: 'de',   label: 'Deutsch' },
    { id: 'it',   label: 'Italiano' },
    { id: 'pt',   label: 'Português' },
    { id: 'nl',   label: 'Nederlands' },
    { id: 'sv',   label: 'Svenska' },
    { id: 'da',   label: 'Dansk' },
    { id: 'nb',   label: 'Norsk bokmål' },
    { id: 'fi',   label: 'Suomi' },
    { id: 'pl',   label: 'Polski' },
    { id: 'cs',   label: 'Čeština' },
    { id: 'hu',   label: 'Magyar' },
    { id: 'ro',   label: 'Română' },
    { id: 'ru',   label: 'Русский' },
    { id: 'uk',   label: 'Українська' },
    { id: 'el',   label: 'Ελληνικά' },
    { id: 'tr',   label: 'Türkçe' },
    { id: 'hi',   label: 'हिन्दी' },
    { id: 'vi',   label: 'Tiếng Việt' },
    { id: 'ja',   label: '日本語' },
    { id: 'ko',   label: '한국어' },
    { id: 'zh',   label: '中文 (简体)' },
  ]

  let bwDownloadMbps = $state('')   // string so '' = unlimited displays cleanly
  let logLevel = $state('info')
  let theme    = $state('cyber')
  let locale   = $state('auto')
  let restoreSparse = $state(true)
  let busy     = $state(false)
  let savedAt  = $state(0)

  onMount(async () => {
    const s = await api.getSettings()
    bwDownloadMbps = s.download_bandwidth_kbps === 0
      ? ''
      : String(Math.round(s.download_bandwidth_kbps / 1000))
    logLevel = s.log_level
    theme    = s.theme
    locale   = s.locale || 'auto'
    restoreSparse = s.restore_sparse ?? true
    document.documentElement.dataset.theme = theme
  })

  function applyTheme(t: string) {
    theme = t
    document.documentElement.dataset.theme = t
  }

  async function save() {
    busy = true
    try {
      const raw = (bwDownloadMbps ?? '').toString().trim()
      const mbps = raw === '' ? 0 : Number(raw)
      const kbps = Number.isFinite(mbps) && mbps > 0 ? Math.round(mbps * 1000) : 0
      const s: Settings = {
        download_bandwidth_kbps: kbps,
        log_level: logLevel,
        theme,
        locale,
        restore_sparse: restoreSparse,
      }
      await api.saveSettings(s)
      savedAt = Date.now()
    } finally {
      busy = false
    }
  }
</script>

<div class="max-w-xl mx-auto p-6 flex flex-col gap-4">
  <header class="flex items-center justify-between">
    <h1 class="text-xl font-semibold">{t('gui.recover.settings.title')}</h1>
    <button
      onclick={onClose}
      class="text-xs px-3 py-1.5 rounded border border-nyx-border text-nyx-muted
             hover:text-nyx-text transition-colors"
    >{t('gui.action.close')}</button>
  </header>

  <section class="rounded-lg border border-nyx-border bg-nyx-surface p-4 flex flex-col gap-4">
    <div class="flex flex-col gap-1 text-xs">
      <span class="text-nyx-muted">{t('gui.recover.settings.bandwidth_label')}</span>
      <input
        type="text"
        inputmode="numeric"
        placeholder={t('gui.recover.settings.unlimited')}
        bind:value={bwDownloadMbps}
        class="bg-nyx-bg border border-nyx-border rounded px-2 py-1.5 text-xs font-mono"
      />
      <div class="flex flex-wrap gap-1 mt-0.5">
        <button
          onclick={() => (bwDownloadMbps = '')}
          class="px-1.5 py-0.5 text-[10px] rounded border border-nyx-border
                 text-nyx-muted hover:text-nyx-text hover:border-nyx-accent
                 transition-colors {bwDownloadMbps.trim() === '' ? 'border-nyx-accent text-nyx-accent' : ''}"
        >{t('gui.recover.settings.unlimited')}</button>
        {#each BW_PRESETS as p (p)}
          <button
            onclick={() => (bwDownloadMbps = String(p))}
            class="px-1.5 py-0.5 text-[10px] rounded border border-nyx-border
                   text-nyx-muted hover:text-nyx-text hover:border-nyx-accent
                   transition-colors {bwDownloadMbps === String(p) ? 'border-nyx-accent text-nyx-accent' : ''}"
          >{p}</button>
        {/each}
      </div>
      <span class="text-[10px] text-nyx-muted">
        {t('gui.recover.settings.bandwidth_desc')}
      </span>
    </div>

    <label class="flex flex-col gap-1 text-xs">
      <span class="text-nyx-muted">{t('gui.recover.settings.language')}</span>
      <select
        bind:value={locale}
        onchange={() => setLocaleOverride(locale)}
        class="bg-nyx-bg border border-nyx-border rounded px-2 py-1.5 text-xs"
      >
        {#each LANGUAGES as l (l.id)}
          <option value={l.id}>{l.label}</option>
        {/each}
      </select>
      <span class="text-[10px] text-nyx-muted">
        {t('gui.recover.settings.language_desc')}
      </span>
    </label>

    <label class="flex flex-col gap-1 text-xs">
      <span class="text-nyx-muted">{t('gui.recover.settings.log_level')}</span>
      <select
        bind:value={logLevel}
        class="bg-nyx-bg border border-nyx-border rounded px-2 py-1.5 text-xs"
      >
        {#each LOG_LEVELS as l (l)}
          <option value={l}>{l}</option>
        {/each}
      </select>
      <span class="text-[10px] text-nyx-muted">
        {t('gui.recover.settings.log_desc')}
      </span>
    </label>

    <label class="flex flex-col gap-1 text-xs">
      <span class="text-nyx-muted">{t('gui.recover.settings.theme')}</span>
      <div class="grid grid-cols-2 gap-1">
        {#each THEMES as th (th.id)}
          <button
            onclick={() => applyTheme(th.id)}
            class="text-xs px-3 py-1.5 rounded border transition-colors
                   {theme === th.id
                     ? 'border-nyx-accent bg-nyx-surface2 text-nyx-text'
                     : 'border-nyx-border text-nyx-muted hover:text-nyx-text'}"
          >{th.label}</button>
        {/each}
      </div>
    </label>

    <label class="flex items-start gap-2 text-xs">
      <input
        type="checkbox"
        bind:checked={restoreSparse}
        class="mt-0.5 accent-nyx-accent"
      />
      <span class="flex flex-col gap-0.5">
        <span class="text-nyx-text">{t('gui.recover.settings.sparse_label')}</span>
        <span class="text-[10px] text-nyx-muted">{t('gui.recover.settings.sparse_desc')}</span>
      </span>
    </label>
  </section>

  <div class="flex items-center gap-2">
    {#if savedAt > 0}
      <span class="text-[10px] text-nyx-success">{t('gui.recover.settings.saved')}</span>
    {/if}
    <button
      onclick={save}
      disabled={busy}
      class="ml-auto text-sm px-3 py-2 rounded bg-nyx-accent text-nyx-bg font-semibold
             hover:bg-nyx-accent-hi disabled:opacity-40 transition-colors"
    >{busy ? t('gui.recover.settings.saving') : t('gui.action.save')}</button>
  </div>
</div>
