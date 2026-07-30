<!--
  Copyright (c) 2026 Nyx Software, LLC
  SPDX-License-Identifier: Apache-2.0
  Nyx Backup Recovery - https://nyxbackup.com

  Unlock screen: paste master key hex (Mode A) or load from `KEY=<hex>`
  file (Mode A from disk).  Mode B (passphrase + bootstrap record) lands
  in stage 3.
-->
<script lang="ts">
  import { open as openDialog } from '@tauri-apps/plugin-dialog'
  import { api } from '../lib/api'
  import { t, tf } from '../lib/i18n.svelte.ts'
  import { friendlyError } from '../lib/errors'

  let { onUnlocked, onBack } = $props<{ onUnlocked: () => void; onBack: () => void }>()

  let keyText = $state('')
  let busy = $state(false)
  let errorMsg = $state('')

  async function loadFromFile() {
    try {
      const sel = await openDialog({
        directory: false,
        multiple: false,
        title: t('gui.recover.unlock.file_dialog_title'),
        filters: [{ name: 'Key files', extensions: ['key', 'txt'] }],
      })
      if (typeof sel !== 'string') return
      // The Recovery Tool has no fs API enabled by default; ask the daemon
      // command-side to read the file once we wire that command in.  For
      // now: tell the user to paste.  (Stage 2 keeps this simple; the
      // file-load helper lands when we add the matching Tauri command.)
      errorMsg = tf('gui.recover.unlock.file_picked', { path: sel })
    } catch (e) {
      errorMsg = friendlyError(e)
    }
  }

  async function handleUnlock() {
    errorMsg = ''
    if (!keyText.trim()) { errorMsg = t('gui.recover.unlock.err_empty'); return }
    busy = true
    try {
      await api.unlock(keyText)
      keyText = ''  // wipe from the input field on success
      onUnlocked()
    } catch (e) {
      errorMsg = friendlyError(e)
    } finally {
      busy = false
    }
  }
</script>

<div class="max-w-2xl mx-auto p-6 flex flex-col gap-5">
  <header class="text-center">
    <h1 class="text-xl font-semibold">{t('gui.recover.unlock.title')}</h1>
    <p class="text-xs text-nyx-muted mt-1">
      {@html t('gui.recover.unlock.desc')}
    </p>
  </header>

  <section class="rounded-lg border border-nyx-border bg-nyx-surface p-4 flex flex-col gap-3">
    <label class="flex flex-col gap-1 text-xs">
      <span class="text-nyx-muted">{t('gui.recover.unlock.master_key')}</span>
      <textarea
        bind:value={keyText}
        placeholder={t('gui.recover.unlock.placeholder')}
        rows="3"
        class="bg-nyx-bg border border-nyx-border rounded px-2 py-1.5 text-xs font-mono resize-none"
        autocomplete="off"
        spellcheck="false"
      ></textarea>
    </label>

    <div class="flex items-center gap-2">
      <button
        onclick={loadFromFile}
        type="button"
        class="text-xs px-3 py-1.5 rounded border border-nyx-border text-nyx-muted
               hover:text-nyx-text hover:border-nyx-accent/50 transition-colors"
      >{t('gui.recover.unlock.load_file')}</button>
      <span class="text-[10px] text-nyx-muted">
        {t('gui.recover.unlock.mode_b_note')}
      </span>
    </div>

    {#if errorMsg}
      <p class="text-xs text-nyx-error break-words">{errorMsg}</p>
    {/if}

    <div class="flex items-center gap-2 mt-1">
      <button
        onclick={onBack}
        type="button"
        class="text-xs px-3 py-1.5 rounded border border-nyx-border text-nyx-muted
               hover:text-nyx-text transition-colors"
      >{t('gui.action.back')}</button>
      <button
        onclick={handleUnlock}
        disabled={busy}
        class="ml-auto text-sm px-3 py-2 rounded bg-nyx-accent text-nyx-bg font-semibold
               hover:bg-nyx-accent-hi disabled:opacity-40 transition-colors"
      >{busy ? t('gui.recover.unlock.unlocking') : t('gui.recover.unlock.title')}</button>
    </div>
  </section>
</div>
