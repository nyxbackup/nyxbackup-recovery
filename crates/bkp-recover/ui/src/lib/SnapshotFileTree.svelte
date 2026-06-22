<!-- Copyright (c) 2026 Nyx Software, LLC -->
<!-- SPDX-License-Identifier: Apache-2.0 -->
<!-- Nyx Backup Recovery - https://nyxbackup.com -->
<!--
  SnapshotFileTree - expandable tree browser for restore selection.

  Replaces the old drill-down navigator with a persistent tree view where
  directories expand and collapse in place.  All selection/exclusion semantics
  are unchanged:
    - Checking a folder selects it and all its contents.
    - Unchecking a child of a selected folder adds it to an exclusion set.

  Props:
    files             - flat list of SnapshotFile entries from list_snapshot_files
    selection         - Set<string> of directly-selected paths
    onSelectionChange - called when selection changes
    excludedPaths     - Set<string> of paths excluded under a selected parent
    onExcludedChange  - called when excluded paths change
    highlightPath     - when set, auto-expands ancestors of this path on mount
    onRestoreFile / onCopyPath / onShowVersions - context menu callbacks
-->
<script lang="ts">
  import { t, tf, fmtDateTime } from './i18n.svelte.ts'

  // Local SnapshotFile type matches what rec_list_snapshot_files returns
  // from the recovery Rust side.  Identical shape to the main app's
  // tauri.ts SnapshotFile so the lifted tree component works unchanged.
  interface SnapshotFile {
    path: string
    size: number
    mtime_ns: number
    is_dir: boolean
    is_symlink: boolean
  }

  interface Props {
    files: SnapshotFile[]
    selection: Set<string>
    onSelectionChange?: (s: Set<string>) => void
    excludedPaths?: Set<string>
    onExcludedChange?: (s: Set<string>) => void
    highlightPath?: string
    onRestoreFile?: (path: string) => void
    onCopyPath?: (path: string) => void
    onShowVersions?: (path: string) => void
  }

  let {
    files,
    selection,
    onSelectionChange,
    excludedPaths = new Set<string>(),
    onExcludedChange,
    highlightPath,
    onRestoreFile,
    onCopyPath,
    onShowVersions,
  }: Props = $props()

  // ── Virtualization ───────────────────────────────────────────────────
  // Folders with many entries (e.g. 100K images in one directory) blow up the
  // DOM when every row is rendered.  Windowed rendering keeps the DOM cost
  // constant.  ROW_HEIGHT must match the actual row height of the .flex items
  // in the each-block below; if you change the row padding or text size,
  // re-measure and update here.
  const ROW_HEIGHT = 28
  // Buffer rows above and below the viewport so fast scrolls don't reveal
  // gaps before the reactive recompute lands.
  const VIRTUAL_BUFFER = 12
  let scrollContainer = $state<HTMLDivElement | null>(null)
  let scrollTop       = $state(0)
  let viewportHeight  = $state(600)  // updated on mount + resize

  function onScroll(e: Event) {
    const el = e.currentTarget as HTMLDivElement
    scrollTop = el.scrollTop
  }

  $effect(() => {
    if (!scrollContainer) return
    const el = scrollContainer
    const updateHeight = () => { viewportHeight = el.clientHeight }
    updateHeight()
    const ro = new ResizeObserver(updateHeight)
    ro.observe(el)
    return () => ro.disconnect()
  })

  // ── Tree state ────────────────────────────────────────────────────────────────
  let expandedPaths = $state<Set<string>>(new Set())
  let cursor        = $state(0)

  // Reset tree when the file list changes; auto-expand top-level dirs.
  // If highlightPath is set, also expand every ancestor of that path.
  $effect(() => {
    childrenMap  // depend on childrenMap (which itself depends on files)
    const topDirs = (childrenMap.get('') ?? [])
      .filter(f => f.is_dir)
      .map(f => f.path)

    if (highlightPath) {
      expandedPaths = new Set([...topDirs, ...ancestorsOf(highlightPath)])
    } else {
      expandedPaths = new Set(topDirs)
    }
    cursor = 0
  })

  // ── Context menu ─────────────────────────────────────────────────────────────
  interface ContextMenu { x: number; y: number; entry: SnapshotFile }
  let contextMenu = $state<ContextMenu | null>(null)

  function openContextMenu(e: MouseEvent, entry: SnapshotFile) {
    e.preventDefault()
    contextMenu = { x: e.clientX, y: e.clientY, entry }
  }

  function closeContextMenu() { contextMenu = null }

  function ctxRestore() {
    if (!contextMenu) return
    onRestoreFile?.(contextMenu.entry.path)
    closeContextMenu()
  }

  function ctxCopyPath() {
    if (!contextMenu) return
    navigator.clipboard.writeText(contextMenu.entry.path).catch(() => {})
    onCopyPath?.(contextMenu.entry.path)
    closeContextMenu()
  }

  function ctxVersionHistory() {
    if (!contextMenu) return
    onShowVersions?.(contextMenu.entry.path)
    closeContextMenu()
  }

  // ── Path helpers ─────────────────────────────────────────────────────────────
  //
  // Snapshot files use whatever separator the OS that wrote the manifest used:
  // backslashes on Windows (`C:\foo\bar.txt`) and forward slashes on
  // macOS/Linux (`/home/alice/foo`).  All helpers below accept BOTH separators
  // so a Windows-authored snapshot opened on the same Windows machine still
  // matches ancestor/descendant relationships.

  /** Last index of either `/` or `\` in `path`; -1 if neither present. */
  function lastSepIndex(path: string): number {
    return Math.max(path.lastIndexOf('/'), path.lastIndexOf('\\'))
  }

  function parentOf(path: string): string {
    const last = lastSepIndex(path)
    if (last <= 0) return ''
    const p = path.substring(0, last)
    // "C:" alone isn't a real folder; promote to root so the tree's root level
    // groups the children rather than nesting them under an empty drive node.
    return /^[A-Za-z]:$/.test(p) ? '' : p
  }

  function ancestorsOf(path: string): string[] {
    const result: string[] = []
    let p = parentOf(path)
    while (p !== '') {
      result.push(p)
      p = parentOf(p)
    }
    return result
  }

  /** True when `child` is strictly inside `parent` (handles both separators). */
  function isPathInside(child: string, parent: string): boolean {
    if (child === parent) return false
    const parentEndsInSep = parent.endsWith('/') || parent.endsWith('\\')
    if (parentEndsInSep) return child.startsWith(parent)
    return child.startsWith(parent + '/') || child.startsWith(parent + '\\')
  }

  function descendantPaths(dirPath: string): string[] {
    return files
      .filter(f => f.path === dirPath || isPathInside(f.path, dirPath))
      .map(f => f.path)
  }

  function isAncestorSelected(path: string): boolean {
    return [...selection].some(s => isPathInside(path, s))
  }

  // ── Children map (O(1) child lookup) ─────────────────────────────────────────
  let childrenMap = $derived((() => {
    const map = new Map<string, SnapshotFile[]>()
    const entryPaths = new Set(files.map(f => f.path))
    for (const f of files) {
      let p = parentOf(f.path)
      // If the computed parent isn't itself an entry in the snapshot (e.g. '/home/user'
      // when the backup set starts at '/home/user/docs'), promote to root level so that
      // walk('', 0) finds it.  This handles Linux/macOS absolute paths correctly.
      if (p !== '' && !entryPaths.has(p)) p = ''
      if (!map.has(p)) map.set(p, [])
      map.get(p)!.push(f)
    }
    for (const children of map.values()) {
      children.sort((a, b) => {
        if (a.is_dir !== b.is_dir) return a.is_dir ? -1 : 1
        return a.path.localeCompare(b.path, undefined, { sensitivity: 'base' })
      })
    }
    return map
  })())

  // ── Flat list of visible tree nodes ──────────────────────────────────────────
  interface TreeNode {
    entry: SnapshotFile
    depth: number
    hasChildren: boolean
  }

  let visibleNodes = $derived((() => {
    const result: TreeNode[] = []
    function walk(parentPath: string, depth: number) {
      for (const entry of childrenMap.get(parentPath) ?? []) {
        const hasChildren = entry.is_dir && (childrenMap.get(entry.path)?.length ?? 0) > 0
        result.push({ entry, depth, hasChildren })
        if (entry.is_dir && expandedPaths.has(entry.path)) {
          walk(entry.path, depth + 1)
        }
      }
    }
    walk('', 0)
    return result
  })())

  // Slice `visibleNodes` to just the rows currently in the viewport (plus a
  // small buffer above and below).  Top/bottom spacer divs in the markup
  // below preserve total scroll height so the scrollbar behaves normally.
  const virtualWindow = $derived((() => {
    const total = visibleNodes.length
    if (total === 0) return { start: 0, end: 0, topPad: 0, bottomPad: 0 }
    const firstVisible = Math.floor(scrollTop / ROW_HEIGHT)
    const visibleCount = Math.ceil(viewportHeight / ROW_HEIGHT)
    const start = Math.max(0, firstVisible - VIRTUAL_BUFFER)
    const end   = Math.min(total, firstVisible + visibleCount + VIRTUAL_BUFFER)
    return {
      start, end,
      topPad: start * ROW_HEIGHT,
      bottomPad: Math.max(0, (total - end) * ROW_HEIGHT),
    }
  })())

  function toggleExpand(path: string) {
    const next = new Set(expandedPaths)
    if (next.has(path)) { next.delete(path) } else { next.add(path) }
    expandedPaths = next
  }

  // ── Selection helpers ─────────────────────────────────────────────────────────
  type CheckState = 'full' | 'partial' | 'none'

  function entryState(entry: SnapshotFile): CheckState {
    const directly  = selection.has(entry.path)
    const inherited = !directly && isAncestorSelected(entry.path)
    const excluded  = excludedPaths.has(entry.path)

    if (excluded) return 'none'
    if (directly || inherited) {
      if (entry.is_dir) {
        const hasExcluded = [...excludedPaths].some(
          ep => isPathInside(ep, entry.path)
        )
        return hasExcluded ? 'partial' : 'full'
      }
      return 'full'
    }
    if (!entry.is_dir) return 'none'

    const paths    = descendantPaths(entry.path)
    const selected = paths.filter(p => selection.has(p) || isAncestorSelected(p)).length
    if (selected === 0) return 'none'
    if (selected === paths.length) return 'full'
    return 'partial'
  }

  function toggle(entry: SnapshotFile) {
    const directly  = selection.has(entry.path)
    const excluded  = excludedPaths.has(entry.path)
    const inherited = !directly && isAncestorSelected(entry.path)

    if (excluded) {
      const next = new Set(excludedPaths)
      next.delete(entry.path)
      onExcludedChange?.(next)
    } else if (directly) {
      const next = new Set(selection)
      next.delete(entry.path)
      onSelectionChange?.(next)
    } else if (inherited) {
      const next = new Set(excludedPaths)
      next.add(entry.path)
      onExcludedChange?.(next)
    } else {
      const next = new Set(selection)
      next.add(entry.path)
      onSelectionChange?.(next)
    }
  }

  // ── Formatting ────────────────────────────────────────────────────────────────
  function humanBytes(b: number): string {
    if (b >= 1_073_741_824) return `${(b / 1_073_741_824).toFixed(1)} GB`
    if (b >= 1_048_576)     return `${(b / 1_048_576).toFixed(1)} MB`
    if (b >= 1024)          return `${(b / 1024).toFixed(1)} KB`
    return `${b} B`
  }

  function formatMtime(ns: number): string {
    if (!ns) return '-'
    return fmtDateTime(ns / 1_000_000, {
      year: '2-digit', month: 'numeric', day: 'numeric',
      hour: 'numeric', minute: '2-digit',
    })
  }

  // ── Keyboard ──────────────────────────────────────────────────────────────────
  function handleKeydown(e: KeyboardEvent) {
    if (e.key === 'Escape') { closeContextMenu(); return }
    const len = visibleNodes.length
    if (e.key === 'ArrowUp') {
      cursor = Math.max(0, cursor - 1)
      e.preventDefault()
    } else if (e.key === 'ArrowDown') {
      cursor = Math.min(len - 1, cursor + 1)
      e.preventDefault()
    } else if (e.key === 'ArrowRight') {
      const node = visibleNodes[cursor]
      if (node?.entry.is_dir && node.hasChildren && !expandedPaths.has(node.entry.path)) {
        toggleExpand(node.entry.path)
      }
      e.preventDefault()
    } else if (e.key === 'ArrowLeft') {
      const node = visibleNodes[cursor]
      if (node?.entry.is_dir && expandedPaths.has(node.entry.path)) {
        toggleExpand(node.entry.path)
      } else if (node) {
        const parentPath = parentOf(node.entry.path)
        const parentIdx  = visibleNodes.findIndex(n => n.entry.path === parentPath)
        if (parentIdx >= 0) cursor = parentIdx
      }
      e.preventDefault()
    } else if (e.key === ' ') {
      const node = visibleNodes[cursor]
      if (node) { e.preventDefault(); toggle(node.entry) }
    } else if (e.key === 'Enter') {
      const node = visibleNodes[cursor]
      if (node?.entry.is_dir) toggleExpand(node.entry.path)
    }
  }
</script>

<!-- svelte-ignore a11y_no_static_element_interactions -->
<div
  class="flex flex-col h-full overflow-hidden relative"
  onkeydown={handleKeydown}
  onclick={closeContextMenu}
  tabindex="-1"
>
  <!-- Header bar -->
  <div class="flex items-center gap-2 px-3 py-2 border-b border-nyx-border bg-nyx-surface shrink-0">
    <span class="text-xs text-nyx-muted flex-1">
      {files.length === 0 ? t('gui.snapshot.select_to_view') : files.length === 1 ? tf('gui.filetree.n_item', { n: 1 }) : tf('gui.filetree.n_items', { n: files.length })}
    </span>
    {#if selection.size > 0}
      <button
        onclick={() => { onSelectionChange?.(new Set()); onExcludedChange?.(new Set()) }}
        class="text-xs text-nyx-accent hover:underline shrink-0"
      >{t('gui.snapshot.clear')} ({selection.size})</button>
    {/if}
  </div>

  <!-- Tree (windowed renderer; only viewport rows live in the DOM) -->
  <div
    class="flex-1 overflow-y-auto"
    bind:this={scrollContainer}
    onscroll={onScroll}
  >
    {#if visibleNodes.length === 0}
      <div class="p-4 text-sm text-nyx-muted">{t('gui.filetree.no_files')}</div>
    {:else}
      {#if virtualWindow.topPad > 0}
        <div style="height: {virtualWindow.topPad}px" aria-hidden="true"></div>
      {/if}
      {#each visibleNodes.slice(virtualWindow.start, virtualWindow.end) as node, sliceIdx (node.entry.path)}
        {@const i         = virtualWindow.start + sliceIdx}
        {@const entry      = node.entry}
        {@const state      = entryState(entry)}
        {@const isExcluded = excludedPaths.has(entry.path)}
        {@const isExpanded = expandedPaths.has(entry.path)}
        {@const name       = entry.path.split('/').pop() ?? entry.path}
        <!-- svelte-ignore a11y_click_events_have_key_events -->
        <div
          role="option"
          aria-selected={i === cursor}
          onclick={() => { cursor = i; if (entry.is_dir) toggleExpand(entry.path) }}
          oncontextmenu={(e) => { cursor = i; openContextMenu(e, entry) }}
          class="flex items-center gap-1.5 pr-2 py-1 text-sm select-none cursor-pointer border-l-2
                 {i === cursor        ? 'bg-nyx-accent/20'     : 'hover:bg-nyx-surface'}
                 {isExcluded          ? 'opacity-50'           : ''}
                 {state === 'partial' ? 'border-nyx-accent/40' : 'border-transparent'}"
          style="padding-left: {node.depth * 16 + 4}px"
        >
          <!-- Expand/collapse triangle (dirs only) -->
          {#if entry.is_dir}
            <button
              onclick={(e) => { e.stopPropagation(); cursor = i; toggleExpand(entry.path) }}
              class="w-4 h-4 flex items-center justify-center shrink-0 text-nyx-muted
                     hover:text-nyx-text text-[10px]"
              tabindex="-1"
              title={isExpanded ? t('gui.action.collapse') : t('gui.action.expand')}
            >{node.hasChildren ? (isExpanded ? '▼' : '▶') : ''}</button>
          {:else}
            <span class="w-4 shrink-0"></span>
          {/if}

          <!-- Checkbox -->
          <input
            type="checkbox"
            checked={state === 'full'}
            onclick={(e) => { e.stopPropagation(); toggle(entry) }}
            class="w-4 h-4 shrink-0 accent-nyx-accent cursor-pointer"
            aria-label="{isExcluded ? t('gui.action.re_include') : t('gui.action.include')} {name}"
          />

          <!-- Icon + name -->
          <span class="text-base leading-none shrink-0">
            {entry.is_dir ? '📁' : entry.is_symlink ? '🔗' : '📄'}
          </span>
          <span class="flex-1 truncate
                       {isExcluded   ? 'text-nyx-muted line-through' :
                        entry.is_dir ? 'text-nyx-text'               :
                                       'text-nyx-muted'}">
            {name}
          </span>
          {#if state === 'partial'}
            <span class="text-xs text-nyx-accent/60 shrink-0 ml-1" title={t('gui.filetree.some_selected')}>◐</span>
          {/if}

          <!-- Modified date -->
          <span class="text-xs text-nyx-muted shrink-0 w-[8rem] text-right tabular-nums">
            {formatMtime(entry.mtime_ns)}
          </span>

          <!-- Size -->
          <span class="text-xs shrink-0 w-14 text-right tabular-nums
                       {!entry.is_dir && !isExcluded && entry.size > 0 ? 'text-nyx-muted' : 'text-transparent'}">
            {!entry.is_dir && entry.size > 0 ? humanBytes(entry.size) : '-'}
          </span>

          <!-- Right badge -->
          {#if isExcluded}
            <span class="text-xs text-nyx-muted border border-nyx-border rounded px-1 shrink-0">excluded</span>
          {:else if state === 'full'}
            <span class="text-xs text-nyx-accent shrink-0 w-4 text-center">✓</span>
          {:else}
            <span class="w-4 shrink-0"></span>
          {/if}
        </div>
      {/each}
      {#if virtualWindow.bottomPad > 0}
        <div style="height: {virtualWindow.bottomPad}px" aria-hidden="true"></div>
      {/if}
    {/if}
  </div>

  <!-- Footer hint -->
  <div class="shrink-0 px-3 py-1.5 border-t border-nyx-border bg-nyx-surface text-xs text-nyx-muted">
    {t('gui.filetree.hint')}
  </div>

  <!-- Context menu -->
  {#if contextMenu}
    <!-- svelte-ignore a11y_click_events_have_key_events -->
    <div
      role="menu"
      class="absolute z-50 min-w-[160px] rounded-lg border border-nyx-border
             bg-nyx-surface shadow-lg py-1 text-sm"
      style="left: {contextMenu.x}px; top: {contextMenu.y}px"
      onclick={(e) => e.stopPropagation()}
    >
      <div class="px-3 py-1 text-xs text-nyx-muted truncate max-w-[220px] border-b border-nyx-border/50 mb-1">
        {contextMenu.entry.path.split('/').pop() ?? contextMenu.entry.path}
      </div>
      {#if onRestoreFile}
        <button
          role="menuitem"
          onclick={ctxRestore}
          class="w-full text-left px-3 py-1.5 hover:bg-nyx-accent/10 text-nyx-text"
        >{contextMenu.entry.is_dir ? t('gui.snapshot.ctx_restore_folder') : t('gui.snapshot.ctx_restore_file')}</button>
      {/if}
      <button
        role="menuitem"
        onclick={ctxCopyPath}
        class="w-full text-left px-3 py-1.5 hover:bg-nyx-accent/10 text-nyx-text"
      >{t('gui.snapshot.ctx_copy_path')}</button>
      {#if onShowVersions}
        <button
          role="menuitem"
          onclick={ctxVersionHistory}
          class="w-full text-left px-3 py-1.5 hover:bg-nyx-accent/10 text-nyx-text"
        >{t('gui.snapshot.ctx_versions')}</button>
      {/if}
    </div>
  {/if}
</div>
