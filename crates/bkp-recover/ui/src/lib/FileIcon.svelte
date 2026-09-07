<script lang="ts">
  // Inline SVG file-type icons (ASCII-only source, self-contained, CSP-safe),
  // so they render identically on Windows/macOS/Linux/WSL - the emoji glyphs
  // they replaced showed as missing-glyph boxes where no colour-emoji font was
  // present (e.g. WSL Ubuntu).  Hybrid treatment: a warm gold folder anchors
  // the eye, files recede in a soft slate - easier to scan a long tree.
  let { kind }: { kind: 'dir' | 'file' | 'symlink' } = $props()

  const GOLD = '#e8b64c'
  const GOLD_EDGE = '#c9992f'
  const SLATE = '#8fb0d8'
</script>

{#if kind === 'dir'}
  <svg
    class="w-4 h-4 shrink-0"
    viewBox="0 0 24 24"
    fill={GOLD}
    stroke={GOLD_EDGE}
    stroke-width="1.4"
    stroke-linejoin="round"
    aria-hidden="true"
  >
    <path d="M20 20a2 2 0 0 0 2-2V8a2 2 0 0 0-2-2h-7.9a2 2 0 0 1-1.69-.9L9.6 3.9A2 2 0 0 0 7.93 3H4a2 2 0 0 0-2 2v13a2 2 0 0 0 2 2Z" />
  </svg>
{:else if kind === 'symlink'}
  <svg
    class="w-4 h-4 shrink-0"
    viewBox="0 0 24 24"
    fill="none"
    stroke={SLATE}
    stroke-width="2"
    stroke-linecap="round"
    stroke-linejoin="round"
    aria-hidden="true"
  >
    <path d="M9 17H7A5 5 0 0 1 7 7h2" />
    <path d="M15 7h2a5 5 0 1 1 0 10h-2" />
    <line x1="8" y1="12" x2="16" y2="12" />
  </svg>
{:else}
  <svg
    class="w-4 h-4 shrink-0"
    viewBox="0 0 24 24"
    fill={SLATE}
    fill-opacity="0.16"
    stroke={SLATE}
    stroke-width="1.8"
    stroke-linecap="round"
    stroke-linejoin="round"
    aria-hidden="true"
  >
    <path d="M15 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V7Z" />
    <path d="M14 2v4a2 2 0 0 0 2 2h4" fill="none" />
  </svg>
{/if}
