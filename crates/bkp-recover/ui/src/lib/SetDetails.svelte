<!-- Copyright (c) 2026 Nyx Software, LLC -->
<!-- SPDX-License-Identifier: Apache-2.0 -->
<!-- Nyx Backup Recovery - https://nyxbackup.com -->
<script lang="ts">
  // Collapsible panel showing a backup set's descriptive metadata, read from
  // its encrypted set-record (REQ-060).  Purely informational; renders nothing
  // when the set has no set-record (older sets, or not run since the feature
  // shipped) or it cannot be read.
  import { api, type SetRecordView } from './api'
  import { t, fmtDateTime } from './i18n.svelte.ts'

  let { setId }: { setId: string | undefined } = $props()

  let rec = $state<SetRecordView | null>(null)
  let open = $state(false)
  // Plain let (not $state): effect bookkeeping, never read from markup, so it
  // must not make the effect re-fire on its own write.
  let fetchedId = ''

  $effect(() => {
    const id = setId
    if (!id) { rec = null; return }
    if (id === fetchedId) return
    fetchedId = id
    rec = null
    api.getSetRecord(id)
      .then((r) => { if (fetchedId === id) rec = r })
      .catch(() => { if (fetchedId === id) rec = null })
  })

  function ownerLine(r: SetRecordView): string {
    const host = r.hostname || '?'
    return r.username ? `${r.username}@${host}` : host
  }
  function retentionLine(r: SetRecordView): string {
    const base = `${r.retention_keep_all_days}d, ${r.retention_keep_weekly_count}w, ${r.retention_keep_monthly_count}m`
    return r.retention_auto_delete ? `${base} (auto-delete)` : base
  }
  function scheduleLine(r: SetRecordView): string {
    if (r.schedule_kind === 'every_n_hours') return `every ${r.schedule_interval_hours}h`
    const extra = [r.schedule_day, r.schedule_time].filter(Boolean).join(' ')
    return extra ? `${r.schedule_kind} ${extra}` : r.schedule_kind
  }
</script>

{#if rec}
  <div class="border-b border-nyx-border bg-nyx-surface2/40 text-xs shrink-0">
    <button
      onclick={() => (open = !open)}
      class="w-full flex items-center justify-between px-3 py-1.5 text-nyx-muted hover:text-nyx-text"
      aria-expanded={open}
    >
      <span class="uppercase tracking-wider font-semibold">{t('gui.recover.browse.set_details.title')}</span>
      <span aria-hidden="true" class="font-mono">{open ? '-' : '+'}</span>
    </button>
    {#if open}
      <dl class="px-3 pb-2 grid grid-cols-[auto_1fr] gap-x-3 gap-y-1">
        <dt class="text-nyx-muted/70">{t('gui.recover.browse.set_details.owner')}</dt>
        <dd class="text-nyx-text truncate" title={ownerLine(rec)}>{ownerLine(rec)}</dd>

        <dt class="text-nyx-muted/70">{t('gui.recover.browse.set_details.destination')}</dt>
        <dd class="text-nyx-text truncate" title={rec.endpoint_target}>{rec.endpoint_target || rec.endpoint_type}</dd>

        <dt class="text-nyx-muted/70">{t('gui.recover.browse.set_details.retention')}</dt>
        <dd class="text-nyx-text">{retentionLine(rec)}</dd>

        <dt class="text-nyx-muted/70">{t('gui.recover.browse.set_details.schedule')}</dt>
        <dd class="text-nyx-text">{scheduleLine(rec)}</dd>

        <dt class="text-nyx-muted/70">{t('gui.recover.browse.set_details.updated')}</dt>
        <dd class="text-nyx-text">{fmtDateTime(rec.updated_at * 1000)}</dd>

        {#if rec.include_paths.length > 0}
          <dt class="text-nyx-muted/70 self-start">{t('gui.recover.browse.set_details.paths')}</dt>
          <dd class="text-nyx-text min-w-0">
            {#each rec.include_paths as p}
              <div class="font-mono text-[11px] truncate" title={p}>{p}</div>
            {/each}
          </dd>
        {/if}
      </dl>
    {/if}
  </div>
{/if}
