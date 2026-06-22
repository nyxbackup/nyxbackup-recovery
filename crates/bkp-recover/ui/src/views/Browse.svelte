<!--
  Copyright (c) 2026 Nyx Software, LLC
  SPDX-License-Identifier: Apache-2.0
  Nyx Backup Recovery - https://nyxbackup.com

  Browse screen.  Visually matches the main app's SnapshotBrowser:
  - Snapshots column on the left (set label + date + size).
  - Centre column hosts the lifted SnapshotFileTree component.
  - Right column has destination + restore controls + Pause / Resume /
    Cancel buttons + live "Selected: N · Free at dest: F of T" preview.
-->
<script lang="ts">
  import { onMount } from 'svelte'
  import { open as openDialog } from '@tauri-apps/plugin-dialog'
  import { invoke } from '@tauri-apps/api/core'
  import { api, type SnapshotSummary } from '../lib/api'
  import SnapshotFileTree from '../lib/SnapshotFileTree.svelte'
  import { t, tf, fmtDateTime } from '../lib/i18n.svelte.ts'
  import { friendlyError } from '../lib/errors'

  let { onBack } = $props<{ onBack: () => void }>()

  let snapshots = $state<SnapshotSummary[]>([])
  let loadingSnaps = $state(false)
  let snapsError = $state('')

  let selectedSnap = $state<SnapshotSummary | null>(null)

  let files = $state<{ path: string; size: number; mtime_ns: number; is_dir: boolean; is_symlink: boolean }[]>([])
  let loadingFiles = $state(false)
  let filesError = $state('')

  let fileSelection = $state(new Set<string>())
  let fileExclusions = $state(new Set<string>())

  // Destination + restore state.  Same shape as bkp-gui's SnapshotBrowser:
  // `destType` toggles between 'desktop' (resolved by the Rust side via
  // rec_local_desktop, matching the main app) and 'custom' (user-picked).
  type DestType = 'desktop' | 'custom'
  let destType = $state<DestType>('desktop')
  let customPath = $state('')
  let defaultDestBase = $state('')

  onMount(async () => {
    await loadSnapshots()
    try { defaultDestBase = await api.localDesktop() } catch { defaultDestBase = '' }
  })

  // Build the per-restore NyxRestore-<snapshot-ts> subfolder using the
  // SNAPSHOT's creation time (not the wall clock) so the user can tell
  // which snapshot a restore came from at a glance.  Matches main app
  // SnapshotBrowser.svelte ~line 660.
  function nyxRestoreSuffix(): string {
    if (!selectedSnap) return 'NyxRestore'
    const dt = new Date(Number(selectedSnap.created_at) * 1000)
    const p = (n: number) => n.toString().padStart(2, '0')
    const ts = `${dt.getFullYear()}-${p(dt.getMonth()+1)}-${p(dt.getDate())}-${p(dt.getHours())}.${p(dt.getMinutes())}.${p(dt.getSeconds())}`
    return `NyxRestore-${ts}`
  }

  function effectiveDestPath(): string {
    const suffix = nyxRestoreSuffix()
    if (destType === 'desktop') {
      if (!defaultDestBase) return ''
      const sep = defaultDestBase.includes('\\') ? '\\' : '/'
      return `${defaultDestBase}${sep}${suffix}`
    }
    const base = customPath.trim().replace(/[\\/]+$/, '')
    if (!base) return ''
    const sep = base.includes('\\') ? '\\' : '/'
    return `${base}${sep}${suffix}`
  }

  let destPath = $state('')
  let restoring = $state(false)
  let restoreErr = $state('')
  let restoreDone = $state(false)
  let restoreCancelled = $state(false)
  let progress = $state<{
    status: string
    files_done: number
    files_total: number
    bytes_done: number
    bytes_total: number
    current_file: string
    error_detail: string
    paused: boolean
  } | null>(null)
  let pollTimer: ReturnType<typeof setInterval> | null = null

  // Pre-restore preview.  Same shape as main SnapshotBrowser: sum bytes
  // of every selected leaf (respecting exclusions), then probe the
  // destination volume's free space.
  function pathMatchesAny(p: string, patterns: Set<string>, allowParentDir: boolean): boolean {
    if (patterns.size === 0) return false
    const norm = p.replace(/\\/g, '/').replace(/^\/+/, '')
    for (const raw of patterns) {
      const pat = raw.replace(/\\/g, '/').replace(/^\/+/, '')
      if (norm === pat) return true
      if (norm.startsWith(pat + '/')) return true
      if (allowParentDir && pat.startsWith(norm + '/')) return true
    }
    return false
  }
  const selectedPreview = $derived.by(() => {
    if (loadingFiles) return { bytes: 0, count: 0 }
    let bytes = 0
    let count = 0
    for (const f of files) {
      if (f.is_dir) continue
      const include = fileSelection.size === 0
        ? false
        : pathMatchesAny(f.path, fileSelection, true)
      if (!include) continue
      if (fileExclusions.size > 0 && pathMatchesAny(f.path, fileExclusions, false)) continue
      bytes += Number(f.size) || 0
      count += 1
    }
    return { bytes, count }
  })

  let destFreeBytes = $state(0)
  let destTotalBytes = $state(0)
  let destFreeDeterminable = $state(false)
  let destFreeProbed = $state(false)
  let destFreeLoading = $state(false)
  let _lastProbedPath = ''
  let _destProbeTimer: ReturnType<typeof setTimeout> | null = null
  async function probeDestFreeSpace(probePath: string): Promise<void> {
    if (!probePath) {
      destFreeProbed = false
      destFreeDeterminable = false
      destFreeBytes = 0
      destTotalBytes = 0
      return
    }
    _lastProbedPath = probePath
    destFreeLoading = true
    try {
      const r = await api.getFreeSpace(probePath)
      if (_lastProbedPath !== probePath) return
      destFreeBytes = Number(r.free_bytes) || 0
      destTotalBytes = Number(r.total_bytes) || 0
      destFreeDeterminable = Boolean(r.determinable)
      destFreeProbed = true
    } catch {
      destFreeProbed = false
      destFreeDeterminable = false
    } finally {
      destFreeLoading = false
    }
  }
  $effect(() => {
    const dt = destType
    const cp = customPath.trim()
    const base = defaultDestBase
    if (_destProbeTimer !== null) clearTimeout(_destProbeTimer)
    _destProbeTimer = setTimeout(() => {
      const probePath = dt === 'desktop' ? base : cp
      probeDestFreeSpace(probePath)
    }, 250)
  })

  const insufficientSpace = $derived.by(() => {
    if (!destFreeProbed || !destFreeDeterminable) return false
    if (selectedPreview.bytes === 0) return false
    return destFreeBytes < selectedPreview.bytes
  })

  // Lock the snapshot picker + file tree while a restore is actively
  // running, mirroring main app SnapshotBrowser.svelte selectionLocked.
  const selectionLocked = $derived(restoring && !restoreDone && !restoreCancelled)

  // Set-label resolver.  Prefers Manifest.set_name; falls
  // back to hostname; finally "Set N" by first-appearance order so
  // multi-set sessions stay distinguishable.
  let setOrdinal = $state<Record<string, string>>({})
  function setLabel(s: SnapshotSummary): string {
    if (s.set_name && s.set_name.trim()) return s.set_name.trim()
    // Older Windows manifests recorded the literal "unknown" as
    // hostname (HOSTNAME env var is Linux-only); treat that as missing.
    const host = (s.hostname ?? '').trim()
    if (host && host.toLowerCase() !== 'unknown') return host
    return setOrdinal[s.set_id] ?? `Set (${s.set_id.slice(0, 8)})`
  }

  async function loadSnapshots() {
    loadingSnaps = true
    snapsError = ''
    try {
      snapshots = await api.listSnapshots()
      const labels: Record<string, string> = {}
      let n = 0
      for (const s of snapshots) {
        if (!labels[s.set_id]) { n += 1; labels[s.set_id] = tf('gui.recover.browse.set_n', { n }) }
      }
      setOrdinal = labels
      if (snapshots.length > 0 && !selectedSnap) await pickSnapshot(snapshots[0])
    } catch (e) {
      snapsError = friendlyError(e)
    } finally {
      loadingSnaps = false
    }
  }

  async function pickSnapshot(s: SnapshotSummary) {
    if (selectionLocked) return
    selectedSnap = s
    fileSelection = new Set()
    fileExclusions = new Set()
    files = []
    loadingFiles = true
    filesError = ''
    try {
      files = await invoke<typeof files>('rec_list_snapshot_files', {
        args: { set_id: s.set_id, snapshot_id: s.snapshot_id },
      })
      // Default to all root-level entries selected (matches main app).
      const roots = files.filter(f =>
        !files.some(o => o.path !== f.path && f.path.startsWith(o.path + '/'))
      )
      fileSelection = new Set(roots.map(f => f.path))
    } catch (e) {
      filesError = friendlyError(e)
    } finally {
      loadingFiles = false
    }
  }

  async function pickDestination() {
    try {
      const sel = await openDialog({ directory: true, title: t('gui.recover.browse.dlg_dest') })
      if (typeof sel === 'string') customPath = sel
    } catch (e) {
      restoreErr = friendlyError(e)
    }
  }

  async function startRestore() {
    restoreErr = ''
    restoreCancelled = false
    if (!selectedSnap) { restoreErr = t('gui.recover.browse.err_no_snapshot'); return }
    if (fileSelection.size === 0) { restoreErr = t('gui.recover.browse.err_no_files'); return }
    destPath = effectiveDestPath()
    if (!destPath) {
      restoreErr = destType === 'custom'
        ? t('gui.recover.browse.pick_dest')
        : t('gui.recover.browse.no_dest_default')
      return
    }
    if (insufficientSpace) {
      restoreErr = t('gui.recover.browse.err_no_space')
      return
    }
    restoring = true
    restoreDone = false
    progress = null
    try {
      await invoke('rec_start_restore', {
        args: {
          set_id:        selectedSnap.set_id,
          snapshot_id:   selectedSnap.snapshot_id,
          dest_path:     destPath,
          filter_paths:  [...fileSelection],
          excluded_paths: [...fileExclusions],
        },
      })
      pollTimer = setInterval(async () => {
        try {
          const p = await invoke<typeof progress>('rec_get_progress')
          progress = p
          if (p && (p.status === 'complete' || p.status === 'error' || p.status === 'cancelled')) {
            if (pollTimer) clearInterval(pollTimer)
            pollTimer = null
            restoring = false
            if (p.status === 'complete') restoreDone = true
            else if (p.status === 'cancelled') restoreCancelled = true
            else restoreErr = p.error_detail ? friendlyError(p.error_detail) : t('gui.recover.browse.err_restore_failed')
          }
        } catch (e) {
          if (pollTimer) clearInterval(pollTimer)
          pollTimer = null
          restoring = false
          restoreErr = friendlyError(e)
        }
      }, 500)
    } catch (e) {
      restoreErr = friendlyError(e)
      restoring = false
    }
  }

  async function pauseRestore()  { try { await api.pauseRestore()  } catch (e) { restoreErr = friendlyError(e) } }
  async function resumeRestore() { try { await api.resumeRestore() } catch (e) { restoreErr = friendlyError(e) } }
  async function cancelRestore() { try { await api.cancelRestore() } catch (e) { restoreErr = friendlyError(e) } }

  function formatDate(secs: string | number | null | undefined): string {
    if (secs === null || secs === undefined) return '-'
    const n = Number(secs)
    if (!n) return '-'
    return fmtDateTime(n * 1000)
  }

  function humanBytes(b: number): string {
    if (b >= 1_073_741_824) return `${(b / 1_073_741_824).toFixed(1)} GB`
    if (b >= 1_048_576) return `${(b / 1_048_576).toFixed(1)} MB`
    if (b >= 1024) return `${(b / 1024).toFixed(1)} KB`
    return `${b} B`
  }
</script>

<div class="grid grid-cols-[260px_1fr_320px] gap-3 p-4 h-full">
  <!-- ── Left: Snapshots ───────────────────────────────────────────────── -->
  <div class="flex flex-col bg-nyx-surface border border-nyx-border rounded-lg overflow-hidden">
    <div class="px-3 py-2 border-b border-nyx-border flex items-center justify-between bg-nyx-surface">
      <span class="text-xs font-semibold text-nyx-muted uppercase tracking-wider">{t('gui.recover.browse.snapshots')}</span>
      <button onclick={loadSnapshots} title={t('gui.recover.browse.refresh')}
              disabled={selectionLocked}
              class="text-[10px] text-nyx-muted hover:text-nyx-text disabled:opacity-40">↻</button>
    </div>
    <div class="flex-1 overflow-auto">
      {#if loadingSnaps}
        <p class="px-3 py-2 text-xs text-nyx-muted">{t('gui.recover.common.loading')}</p>
      {:else if snapsError}
        <p class="px-3 py-2 text-xs text-nyx-error break-words">{snapsError}</p>
      {:else if snapshots.length === 0}
        <p class="px-3 py-2 text-xs text-nyx-muted">{t('gui.recover.browse.no_snapshots')}</p>
      {:else}
        {#each snapshots as snap (snap.snapshot_id)}
          <button
            onclick={() => pickSnapshot(snap)}
            disabled={selectionLocked}
            class="w-full text-left px-3 py-2 text-xs border-b border-nyx-border/50 disabled:opacity-40 disabled:cursor-not-allowed
                   {selectedSnap?.snapshot_id === snap.snapshot_id
                     ? 'bg-nyx-accent/20 text-nyx-text'
                     : 'text-nyx-muted hover:bg-nyx-surface2'}"
          >
            <div class="font-medium truncate" title={setLabel(snap)}>{setLabel(snap)}</div>
            <div class="text-nyx-muted mt-0.5 truncate">{formatDate(snap.created_at)}</div>
            <div class="text-[10px] text-nyx-muted mt-0.5">
              {tf('gui.recover.browse.n_files', { count: Number(snap.files_total).toLocaleString() })} · {humanBytes(Number(snap.bytes_total))}
            </div>
          </button>
        {/each}
      {/if}
    </div>
  </div>

  <!-- ── Centre: File tree ────────────────────────────────────────────── -->
  <div class="flex flex-col bg-nyx-surface border border-nyx-border rounded-lg overflow-hidden
              {selectionLocked ? 'opacity-60 pointer-events-none' : ''}">
    <div class="px-3 py-2 border-b border-nyx-border bg-nyx-surface">
      <span class="text-xs font-semibold text-nyx-muted uppercase tracking-wider">{t('gui.recover.browse.files')}</span>
      {#if fileSelection.size > 0}
        <span class="ml-2 text-nyx-accent text-xs">({tf('gui.recover.browse.n_selected', { count: fileSelection.size })})</span>
      {/if}
      {#if selectionLocked}
        <span class="ml-2 text-[10px] text-nyx-muted">({t('gui.recover.browse.locked_restoring')})</span>
      {/if}
    </div>
    <div class="flex-1 overflow-auto">
      {#if loadingFiles}
        <p class="px-3 py-2 text-xs text-nyx-muted">{t('gui.recover.browse.loading_tree')}</p>
      {:else if filesError}
        <p class="px-3 py-2 text-xs text-nyx-error break-words">{filesError}</p>
      {:else if files.length === 0}
        <p class="px-3 py-2 text-xs text-nyx-muted">{t('gui.recover.browse.select_snapshot')}</p>
      {:else}
        <SnapshotFileTree
          {files}
          selection={fileSelection}
          onSelectionChange={(s) => { fileSelection = s }}
          excludedPaths={fileExclusions}
          onExcludedChange={(s) => { fileExclusions = s }}
        />
      {/if}
    </div>
  </div>

  <!-- ── Right: Destination + restore + progress ──────────────────────── -->
  <div class="flex flex-col bg-nyx-surface border border-nyx-border rounded-lg p-4 gap-3">
    <div class="flex flex-col gap-2">
      <p class="text-xs font-semibold text-nyx-text">{t('gui.recover.browse.restore_to')}</p>

      <label class="flex flex-col gap-0.5 cursor-pointer">
        <span class="flex items-center gap-2 text-sm">
          <input type="radio" bind:group={destType} value="desktop" disabled={selectionLocked} class="accent-nyx-accent" />
          <span class="text-nyx-text">
            {t('gui.recover.browse.default_folder')}{#if defaultDestBase}: <span class="font-mono text-xs">{defaultDestBase}</span>{/if}
          </span>
        </span>
      </label>

      <label class="flex flex-col gap-0.5 cursor-pointer">
        <span class="flex items-center gap-2 text-sm">
          <input type="radio" bind:group={destType} value="custom" disabled={selectionLocked} class="accent-nyx-accent" />
          <span class="text-nyx-text">{t('gui.recover.browse.custom_folder')}</span>
        </span>
      </label>

      {#if destType === 'custom'}
        <div class="mt-1 flex items-center gap-2">
          <button onclick={pickDestination} disabled={selectionLocked}
            class="shrink-0 text-xs px-3 py-1.5 rounded border border-nyx-border
                   text-nyx-muted hover:text-nyx-text hover:border-nyx-accent/50
                   transition-colors whitespace-nowrap disabled:opacity-40">{t('gui.action.browse_fs')}</button>
          <span class="text-xs text-nyx-muted truncate flex-1" title={customPath}>
            {customPath || t('gui.recover.browse.no_folder')}
          </span>
        </div>
      {/if}

      <p class="text-[10px] text-nyx-muted">
        {@html t('gui.recover.browse.crossplatform_note')}
      </p>
    </div>

    <!-- Selected + free-space preview (matches main app right pane). -->
    {#if !loadingFiles && fileSelection.size > 0}
      <div class="text-xs text-nyx-muted leading-relaxed">
        <div>{tf('gui.recover.browse.selected', { count: selectedPreview.count.toLocaleString(), bytes: humanBytes(selectedPreview.bytes) })}</div>
        {#if destFreeProbed && destFreeDeterminable}
          <div class={insufficientSpace ? 'text-red-400' : ''}>
            {tf('gui.recover.browse.free_at_dest', { free: humanBytes(destFreeBytes), total: humanBytes(destTotalBytes) })}
          </div>
          {#if insufficientSpace}
            <div class="text-red-400 mt-1">
              {tf('gui.recover.browse.need_space', { need: humanBytes(selectedPreview.bytes), free: humanBytes(destFreeBytes) })}
            </div>
          {/if}
        {:else if destFreeLoading}
          <div class="text-nyx-subtle italic">{t('gui.recover.browse.checking_dest')}</div>
        {/if}
      </div>
    {:else if !loadingFiles && fileSelection.size === 0}
      <div class="text-xs text-red-400">
        {t('gui.recover.browse.no_files_hint')}
      </div>
    {/if}

    {#if progress && (progress.status === 'running' || progress.status === 'cancelling')}
      {@const total = Number(progress.bytes_total) || 1}
      {@const done = Number(progress.bytes_done) || 0}
      {@const pct = total > 0 ? Math.min(100, Math.round((done / total) * 100)) : 0}
      <div class="flex flex-col gap-1 text-xs">
        <div class="flex justify-between">
          <span>{progress.files_done.toLocaleString()} / {progress.files_total.toLocaleString()}</span>
          <span>{progress.paused ? t('gui.recover.browse.paused') : `${pct}%`}</span>
        </div>
        <div class="h-1.5 bg-nyx-bg rounded overflow-hidden">
          <div class="h-full {progress.paused ? 'bg-nyx-muted' : 'bg-nyx-accent'}" style="width: {pct}%"></div>
        </div>
        <div class="text-[10px] text-nyx-muted truncate" title={progress.current_file}>
          {progress.current_file}
        </div>
        <div class="text-[10px] text-nyx-muted">
          {humanBytes(done)} of {humanBytes(total)}
        </div>
      </div>

      <div class="flex items-center gap-2">
        {#if progress.paused}
          <button onclick={resumeRestore}
            class="text-xs px-3 py-1.5 rounded border border-nyx-accent/50 text-nyx-accent
                   hover:bg-nyx-accent/10 transition-colors">{t('gui.recover.browse.resume')}</button>
        {:else}
          <button onclick={pauseRestore}
            class="text-xs px-3 py-1.5 rounded border border-nyx-border text-nyx-muted
                   hover:text-nyx-text hover:border-nyx-accent/50 transition-colors">{t('gui.recover.browse.pause')}</button>
        {/if}
        <button onclick={cancelRestore}
          class="text-xs px-3 py-1.5 rounded border border-red-500/40 text-red-400
                 hover:bg-red-500/10 transition-colors">{t('gui.action.cancel')}</button>
      </div>
    {/if}

    {#if restoreErr}
      <p class="text-xs text-nyx-error break-words">{restoreErr}</p>
    {/if}
    {#if restoreCancelled}
      <p class="text-xs text-nyx-muted">{t('gui.recover.browse.cancelled')}</p>
    {/if}
    {#if restoreDone}
      <div class="flex flex-col gap-1.5">
        <p class="text-xs text-nyx-success">{t('gui.recover.browse.complete')}</p>
        <p class="text-[10px] text-nyx-muted break-all" title={destPath}>{destPath}</p>
        <button onclick={() => api.openFolder(destPath).catch((e) => { restoreErr = friendlyError(e) })}
          class="self-start text-xs px-3 py-1.5 rounded border border-nyx-border
                 text-nyx-muted hover:text-nyx-text hover:border-nyx-accent
                 transition-colors">{t('gui.recover.browse.open_dest')}</button>
      </div>
    {/if}

    <button
      onclick={startRestore}
      disabled={restoring || !selectedSnap || fileSelection.size === 0
                || (destType === 'custom' && !customPath.trim())
                || insufficientSpace}
      title={insufficientSpace ? t('gui.recover.browse.err_no_space') : ''}
      class="mt-auto text-sm px-3 py-2 rounded-lg bg-nyx-accent text-nyx-bg font-semibold
             hover:bg-nyx-accent-hi disabled:opacity-40 disabled:cursor-not-allowed transition-colors"
    >
      {restoring ? t('gui.recover.browse.restoring') : t('gui.recover.browse.restore_now')}
    </button>
    <button onclick={onBack} type="button" disabled={selectionLocked}
      class="text-xs px-3 py-1.5 rounded-lg border border-nyx-border text-nyx-muted
             hover:text-nyx-text transition-colors disabled:opacity-40">{t('gui.action.back')}</button>
  </div>
</div>
