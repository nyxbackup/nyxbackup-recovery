<!--
  Copyright (c) 2026 Nyx Software, LLC
  SPDX-License-Identifier: Apache-2.0
  Nyx Backup Recovery - https://nyxbackup.com

  About screen.  Reachable via the "?" button on every screen.
-->
<script lang="ts">
  import { onMount } from 'svelte'
  import { open as openExternal } from '@tauri-apps/plugin-shell'
  import { api, type AppInfo } from '../lib/api'
  import { t } from '../lib/i18n.svelte.ts'
  import logoUrl from '../lib/logo.png'

  let { onClose } = $props<{ onClose: () => void }>()
  let info = $state<AppInfo | null>(null)

  onMount(async () => { info = await api.appInfo() })

  function openSite() { openExternal('https://nyxbackup.com/recovery') }
  function openSource() { openExternal('https://github.com/nyxbackup/nyxbackup-recovery') }
</script>

<div class="max-w-xl mx-auto p-6 flex flex-col gap-4">
  <header class="flex items-center justify-between">
    <h1 class="text-xl font-semibold">{t('gui.recover.about.title')}</h1>
    <button
      onclick={onClose}
      class="text-xs px-3 py-1.5 rounded border border-nyx-border text-nyx-muted
             hover:text-nyx-text transition-colors"
    >{t('gui.action.close')}</button>
  </header>

  <section class="rounded-lg border border-nyx-border bg-nyx-surface p-4 flex flex-col gap-2">
    <div class="flex items-center justify-center mb-2">
      <img src={logoUrl} alt="Nyx Backup" class="w-20 h-20" />
    </div>
    {#if info}
      <div class="flex items-baseline justify-between">
        <span class="text-sm font-semibold">{info.name}</span>
        <span class="text-xs font-mono text-nyx-muted">v{info.version}</span>
      </div>
      <p class="text-xs text-nyx-muted">{t('gui.recover.about.tagline')}</p>
      <p class="text-xs text-nyx-muted">{t('gui.recover.about.build_target')} <span class="font-mono">{info.target}</span></p>
    {:else}
      <p class="text-xs text-nyx-muted">{t('gui.recover.common.loading')}</p>
    {/if}

    <hr class="border-nyx-border my-2" />

    <p class="text-xs text-nyx-muted">
      {t('gui.recover.about.blurb')}
    </p>

    <button
      onclick={openSite}
      class="self-start mt-2 text-xs underline text-nyx-accent hover:text-nyx-accent-hi"
    >https://nyxbackup.com/recovery</button>

    <button
      onclick={openSource}
      class="self-start text-xs underline text-nyx-accent hover:text-nyx-accent-hi"
    >{t('gui.recover.about.source')}</button>

    <p class="text-[10px] text-nyx-muted mt-3">
      {t('gui.recover.about.legal')}
    </p>
  </section>
</div>
