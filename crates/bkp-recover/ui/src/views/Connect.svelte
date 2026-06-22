<!--
  Copyright (c) 2026 Nyx Software, LLC
  SPDX-License-Identifier: Apache-2.0
  Nyx Backup Recovery - https://nyxbackup.com

  Connect screen: storage backend + per-backend fields + master key.
  A backend selector plus the per-backend credential fields (backend
  list, order, labels, placeholders, and conditional credential blocks).
  'local' is deliberately omitted (recovery from a local source is a
  plain file copy).

  By design:
  - "Test connection" must succeed before "Continue" enables.
  - The "Master encryption key" card sits below the storage card on the
    same page (a one-page flow because the user has both at hand at the
    same time).
  - No storage-class picker (read-only, irrelevant for recovery).
-->
<script lang="ts">
  import { onMount } from 'svelte'
  import { invoke } from '@tauri-apps/api/core'
  import { open as openDialog } from '@tauri-apps/plugin-dialog'
  import { api, type RecentEndpoint } from '../lib/api'
  import { connectForm, type EndpointType } from '../lib/connect_state.svelte.ts'
  import { t, tf } from '../lib/i18n.svelte.ts'
  import { friendlyError } from '../lib/errors'

  let { onConnected } = $props<{ onConnected: () => void }>()

  // STORAGE_TYPES order and labels lifted verbatim from the main app's
  // BackupSetEditor.svelte ("Storage types" array minus 'local').
  const STORAGE_TYPES: EndpointType[] = [
    's3', 's3_compat', 'azure_blob', 'backblaze_b2', 'dropbox',
    'gcs', 'google_drive', 'onedrive', 'sftp', 'smb', 'webdav', 'local',
  ]
  // $derived so the translatable (non-brand) labels re-render on a live
  // language switch; brand names stay verbatim.
  const STORAGE_LABELS: Record<EndpointType, string> = $derived({
    local:         t('gui.recover.connect.backend_local'),
    s3:            'Amazon S3',
    s3_compat:     'S3-Compatible',
    azure_blob:    'Azure Blob Storage',
    backblaze_b2:  'Backblaze B2',
    gcs:           'Google Cloud Storage',
    google_drive:  'Google Drive',
    onedrive:      'Microsoft OneDrive',
    dropbox:       'Dropbox',
    sftp:          'SFTP',
    smb:           t('gui.recover.connect.backend_smb'),
    webdav:        t('gui.recover.connect.backend_webdav'),
  })
  // urlPlaceholder lifted verbatim from the main editor.
  const urlPlaceholder: Record<EndpointType, string> = {
    local:        '/mnt/nfs/backup  (mount the share first, then point here)',
    s3:           's3://my-bucket/prefix',
    s3_compat:    's3://my-bucket/prefix',
    azure_blob:   'azure://account/container/prefix',
    backblaze_b2: 'b2://bucket/prefix',
    gcs:          'gcs://bucket/prefix',
    google_drive: 'https://drive.google.com/drive/folders/...  or  folder-ID',
    onedrive:     '/NyxBackup',
    dropbox:      '/NyxBackup',
    sftp:         'sftp://user@host/path',
    smb:          'smb://user@host/share/prefix',
    webdav:       'https://nextcloud.example.com/remote.php/dav/files/USER/NyxBackup/',
  }

  // OAuth helpers (#2): use the main app's bundled Tauri commands when
  // available so users get the same "Connect with Google" / "Connect
  // with Dropbox" one-click flow.  The Recovery Tool doesn't bundle
  // its own OAuth client IDs - if the main app's nyx_bkp_gui is
  // installed on the same machine, we can invoke its commands via
  // Tauri's command bus.  When not available, fall back to paste-
  // refresh-token (the existing flow).
  // We ship the paste-token form universally and add a
  // helper-text link explaining how to extract the token from the
  // source machine; a future release will wire the in-process OAuth
  // browser dance.

  let testing       = $state(false)        // "Test connection" in flight
  let connTested    = $state<'idle' | 'ok' | 'fail'>('idle')
  let connMessage   = $state('')
  let busy          = $state(false)        // "Continue" in flight
  let errorMsg      = $state('')
  let recent        = $state<RecentEndpoint[]>([])

  // Manual (no-local-browser) OAuth relay state.  See the "No browser on this
  // machine?" block in the OAuth section.
  let manualAuthUrl     = $state('')
  let manualRedirectUri = $state('')
  let manualPasted      = $state('')
  let manualBusy        = $state(false)

  // OneDrive tenant/account-type selector (matches the main app's editor):
  // common / consumers / organizations / a custom Entra tenant GUID.
  let onedriveTenantChoice = $state<'common' | 'consumers' | 'organizations' | 'custom'>('common')
  let onedriveTenantCustom = $state('')
  let connectingOnedrive   = $state(false)
  const onedriveTenant = $derived(
    onedriveTenantChoice === 'custom' ? (onedriveTenantCustom.trim() || 'common') : onedriveTenantChoice
  )

  onMount(async () => {
    try { recent = await api.getRecent() } catch { recent = [] }
  })

  function fillFromRecent(r: RecentEndpoint) {
    connectForm.storageType   = r.endpoint_type as EndpointType
    // Mark this backend as current so the clear-on-switch effect does NOT
    // fire and wipe the values we restore just below.
    lastBackend = r.endpoint_type as EndpointType
    connectForm.storageUrl    = r.url
    connectForm.storageKeyId  = r.key_id
    // restore the persisted secret + region + endpoint URL.  The
    // recents file already carries them; the previous
    // fillFromRecent just dropped them on the floor.  This is what the
    // user reported as "sec access key and region not retained".
    connectForm.storageSecret = r.secret ?? ''
    connectForm.storageRegion = r.region ?? ''
    connectForm.endpointUrl   = r.endpoint_url ?? ''
    connectForm.label         = r.label
    // Any change invalidates the previous "test connection" result.
    connTested = 'idle'
    connMessage = ''
  }

  // Remove a saved endpoint from the recents list.  The backend returns the
  // updated list so we refresh in place.
  async function removeRecent(r: RecentEndpoint) {
    try {
      recent = await api.removeRecent(r.endpoint_type, r.url, r.key_id)
    } catch (e) {
      errorMsg = friendlyError(e)
    }
  }

  // Manual OAuth relay, step 1: fetch a sign-in URL the user opens on any
  // browser (no browser needed on this machine).
  async function getManualOauthUrl() {
    errorMsg = ''
    manualAuthUrl = ''
    manualRedirectUri = ''
    const cmd = connectForm.storageType === 'dropbox' ? 'rec_dropbox_oauth_url'
      : connectForm.storageType === 'google_drive' ? 'rec_google_oauth_url'
      : 'rec_onedrive_oauth_url'
    const args = connectForm.storageType === 'onedrive' ? { tenantId: onedriveTenant } : {}
    try {
      const r = await invoke<{ auth_url: string; redirect_uri: string }>(cmd, args)
      manualAuthUrl = r.auth_url
      manualRedirectUri = r.redirect_uri
    } catch (e) {
      errorMsg = friendlyError(e)
    }
  }

  // One-click OneDrive sign-in (opens the browser; works in WSL too).
  async function connectOnedrive() {
    errorMsg = ''
    connectingOnedrive = true
    try {
      const r = await invoke<{ secret: string; email: string }>(
        'rec_onedrive_oauth', { tenantId: onedriveTenant })
      connectForm.storageSecret = r.secret
      fieldChanged()
      connMessage = `Connected as ${r.email || '(unknown account)'}.`
    } catch (e) {
      errorMsg = friendlyError(e)
    } finally {
      connectingOnedrive = false
    }
  }

  // Manual OAuth relay, step 2: exchange the pasted redirect URL / code.
  async function finishManualOauth() {
    errorMsg = ''
    manualBusy = true
    try {
      if (connectForm.storageType === 'google_drive') {
        const r = await invoke<{ secret: string; folder_id: string; email: string }>(
          'rec_google_oauth_exchange',
          { folderUrl: connectForm.storageUrl.trim(), pasted: manualPasted, redirectUri: manualRedirectUri })
        connectForm.storageUrl = r.folder_id
        connectForm.storageSecret = r.secret
        connMessage = `Connected as ${r.email || '(unknown account)'}.`
      } else {
        const cmd = connectForm.storageType === 'dropbox'
          ? 'rec_dropbox_oauth_exchange' : 'rec_onedrive_oauth_exchange'
        // `secret` is the storage-secret value (a JSON blob for OneDrive, a
        // bare refresh token for Dropbox).  OneDrive also needs the tenant.
        const exArgs = connectForm.storageType === 'onedrive'
          ? { tenantId: onedriveTenant, pasted: manualPasted, redirectUri: manualRedirectUri }
          : { pasted: manualPasted, redirectUri: manualRedirectUri }
        const r = await invoke<{ secret: string; email: string }>(cmd, exArgs)
        connectForm.storageSecret = r.secret
        connMessage = `Connected as ${r.email || '(unknown account)'}.`
      }
      manualPasted = ''
      manualAuthUrl = ''
      fieldChanged()
    } catch (e) {
      errorMsg = friendlyError(e)
    } finally {
      manualBusy = false
    }
  }

  async function loadKeyFromFile() {
    try {
      const sel = await openDialog({
        directory: false,
        multiple: false,
        title: t('gui.recover.connect.dlg_key_file'),
        filters: [{ name: t('gui.recover.connect.filter_key_files'), extensions: ['key', 'txt'] }],
      })
      if (typeof sel !== 'string') return
      const body = await invoke<string>('rec_read_key_file', { path: sel })
      // Strip lines starting with `#` (comments) and blank lines so the
      // user can annotate key files.
      const lines = body.split(/\r?\n/)
        .map(l => l.trim())
        .filter(l => l.length > 0 && !l.startsWith('#'))
      connectForm.masterKeyText = lines.join('').trim()
    } catch (e) {
      errorMsg = friendlyError(e)
    }
  }

  // Invalidate the test-connection state on any field change so the
  // Continue button doesn't enable on stale results.
  function fieldChanged() {
    connTested = 'idle'
    connMessage = ''
  }

  // Track the last backend type so we can detect a SWITCH and clear the
  // connection fields - their values mean different things per backend, and
  // carrying them across caused mangled inputs (e.g. a B2 URL prepended to a
  // WebDAV URL, or an S3 region appearing as an s3_compat endpoint URL).
  // Everything that identifies/authenticates the endpoint is reset to a clean
  // slate.  The master key is deliberately KEPT - it is the same regardless of
  // which backend hosts the data, and re-typing it every switch is painful.
  let lastBackend = $state<EndpointType>(connectForm.storageType)
  $effect(() => {
    if (connectForm.storageType !== lastBackend) {
      connectForm.storageUrl = ''
      connectForm.storageKeyId = ''
      connectForm.storageSecret = ''
      connectForm.storageRegion = ''
      connectForm.endpointUrl = ''
      connectForm.s3Region = ''
      connectForm.label = ''
      // NOTE: connectForm.masterKeyText is intentionally NOT cleared.
      lastBackend = connectForm.storageType
      // A switch invalidates the previous Test result too.
      connTested = 'idle'
      connMessage = ''
    }
  })

  // Build the args once - reused by both Test and Continue so the two
  // call sites can't drift.
  function connectArgs() {
    return {
      endpoint_type: connectForm.storageType,
      url:           connectForm.storageUrl.trim(),
      key_id:        connectForm.storageKeyId.trim(),
      secret:        connectForm.storageSecret,
      label:         connectForm.label.trim() || null,
      // The `storageRegion` field is the shared producer for whatever each
      // backend stuffs in the `region` slot (the TOML producer reads it
      // there): the AWS region for s3, the endpoint URL for s3_compat, and
      // the mount-path for smb (on Linux/macOS the share is OS-mounted and
      // read from that path).
      region:        connectForm.storageType === 's3_compat'
                       || connectForm.storageType === 's3'
                       || connectForm.storageType === 'smb'
                       ? connectForm.storageRegion.trim()
                       : null,
      // WebDAV's full base URL is the top URL field; forward it in the
      // endpoint_url slot the backend reads (params_to_endpoint_config
      // promotes it for WebDav).  There is no separate base-URL field.
      endpoint_url:  connectForm.storageType === 'webdav'
                       ? connectForm.storageUrl.trim()
                       : null,
      // s3_compat only: an actual S3 region override (the `region` slot above
      // carries the endpoint URL for s3_compat, so the real region travels
      // separately).  Empty falls back to the us-east-1 default.
      s3_region:     connectForm.storageType === 's3_compat'
                       ? (connectForm.s3Region.trim() || null)
                       : null,
      host:          null,
      port:          null,
      username:      null,
      // SFTP private-key file, or WebDAV client-certificate PEM.  For SFTP the
      // secret field is treated as the key passphrase when this is set.
      private_key_path: (connectForm.storageType === 'sftp'
                          || connectForm.storageType === 'webdav')
                          && connectForm.privateKeyPath.trim()
                          ? connectForm.privateKeyPath.trim()
                          : null,
    }
  }

  // Backends that use the separate Key / username field (access key id,
  // storage account name, application key id, or WebDAV user).  Everything
  // else authenticates without it: gcs uses a service-account JSON;
  // google_drive / onedrive / dropbox are OAuth; sftp / smb carry the
  // username inside the URL (e.g. sftp://user@host/path).  Keep this list in
  // sync with the per-backend form fields below - it gates the validation in
  // both validateForm() and handleContinue() so the two can't drift.
  const KEY_ID_BACKENDS = ['s3', 's3_compat', 'backblaze_b2', 'webdav']

  function validateForm(): string | null {
    if (!connectForm.storageUrl.trim()) return t('gui.recover.connect.err_url_required')
    if (KEY_ID_BACKENDS.includes(connectForm.storageType)
        && !connectForm.storageKeyId.trim()) return t('gui.recover.connect.err_key_required')
    // 'local' is a plain folder (mounted NFS/USB/etc.) - no credentials.
    if (connectForm.storageType !== 'local' && !connectForm.storageSecret)
      return t('gui.recover.connect.err_secret_required')
    if (connectForm.storageType === 's3_compat' && !connectForm.storageRegion.trim()) {
      return t('gui.recover.connect.err_endpoint_required')
    }
    // WebDAV uses the top URL field as its full base URL (build_endpoint reads
    // it from `url`); there is no separate base-URL field.
    return null
  }

  async function handleTestConnection() {
    errorMsg = ''
    connMessage = ''
    const validationErr = validateForm()
    if (validationErr) { errorMsg = validationErr; return }
    testing = true
    try {
      await invoke('rec_test_connection', { args: connectArgs() })
      connTested = 'ok'
      connMessage = t('gui.recover.connect.conn_verified')
    } catch (e) {
      connTested = 'fail'
      connMessage = friendlyError(e)
    } finally {
      testing = false
    }
  }

  async function handleContinue() {
    errorMsg = ''
    // List every missing item rather than failing on the first one, so
    // the user sees all required fields at once instead of a vague
    // "Test connection first" that doesn't say WHICH field is missing.
    const missing: string[] = []
    if (!connectForm.storageUrl.trim()) missing.push(t('gui.recover.connect.field_storage_url'))
    if (!connectForm.storageKeyId.trim()
        && KEY_ID_BACKENDS.includes(connectForm.storageType)) {
      missing.push(t('gui.recover.connect.field_key'))
    }
    if (connectForm.storageType !== 'local' && !connectForm.storageSecret) {
      missing.push(t('gui.recover.connect.field_secret'))
    }
    if (connectForm.storageType === 's3_compat' && !connectForm.storageRegion.trim()) {
      missing.push(t('gui.recover.connect.field_endpoint_url'))
    }
    if (connTested !== 'ok') missing.push(t('gui.recover.connect.field_test'))
    if (!connectForm.masterKeyText.trim()) missing.push(t('gui.recover.connect.field_master_key'))

    if (missing.length > 0) {
      errorMsg = t('gui.recover.connect.missing_prefix') + '\n  \u2022 ' + missing.join('\n  \u2022 ')
      return
    }
    busy = true
    try {
      await invoke('rec_connect', { args: connectArgs() })
      await invoke('rec_unlock', { args: { master_key_hex: connectForm.masterKeyText } })
      // Keep the master key in the (in-memory only) form for the process
      // lifetime, exactly like the storage credentials above.  A single
      // machine has ONE master key that decrypts every endpoint, so wiping
      // it here forced a re-import every time the user went Back to pick a
      // different endpoint (reported).  It is cleared on Disconnect, never
      // persisted to disk.
      onConnected()
    } catch (e) {
      errorMsg = friendlyError(e)
    } finally {
      busy = false
    }
  }
</script>

<div class="max-w-3xl mx-auto p-6 flex flex-col gap-5">
  <header class="text-center">
    <h1 class="text-xl font-semibold">{t('gui.recover.connect.title')}</h1>
    <p class="text-xs text-nyx-muted mt-1">
      {t('gui.recover.connect.subtitle')}
    </p>
  </header>

  {#if recent.length > 0}
    <section class="rounded-lg border border-nyx-border bg-nyx-surface p-3">
      <h2 class="text-xs font-semibold text-nyx-muted uppercase tracking-wide mb-2">{t('gui.recover.connect.recently_used')}</h2>
      <ul class="flex flex-col gap-1">
        {#each recent as r (r.endpoint_type + r.url + r.key_id)}
          <li class="flex items-center gap-1">
            <button
              onclick={() => fillFromRecent(r)}
              class="flex-1 text-left px-2 py-1.5 text-xs rounded hover:bg-nyx-surface2 transition-colors"
            >
              <span class="font-mono">{r.endpoint_type}</span>
              <span class="text-nyx-muted">·</span>
              {r.label || r.url}
            </button>
            <button
              onclick={() => removeRecent(r)}
              title={t('gui.recover.connect.remove_recent')}
              aria-label={t('gui.recover.connect.remove_recent')}
              class="shrink-0 px-1.5 py-1.5 rounded text-nyx-muted hover:text-nyx-text
                     hover:bg-nyx-surface2 transition-colors"
            >
              <svg xmlns="http://www.w3.org/2000/svg" width="12" height="12" viewBox="0 0 24 24"
                   fill="none" stroke="currentColor" stroke-width="2.5" stroke-linecap="round">
                <line x1="18" y1="6" x2="6" y2="18" />
                <line x1="6" y1="6" x2="18" y2="18" />
              </svg>
            </button>
          </li>
        {/each}
      </ul>
    </section>
  {/if}

  <!-- ── Storage destination (lifted from main editor) ───────────────────── -->
  <section class="rounded-lg border border-nyx-border bg-nyx-surface p-5 flex flex-col gap-3">
    <span class="text-xs font-medium text-nyx-muted uppercase tracking-wide">{t('gui.recover.connect.storage_dest')}</span>

    <div class="flex flex-col gap-1.5">
      <label for="rec-storage-type" class="text-xs text-nyx-muted">{t('gui.recover.connect.backend')}</label>
      <select
        id="rec-storage-type"
        bind:value={connectForm.storageType}
        onchange={fieldChanged}
        class="bg-nyx-surface2 border border-nyx-border rounded-lg px-3 py-2 text-sm
               text-nyx-text focus:outline-none focus:border-nyx-accent transition-colors
               appearance-none w-full min-w-[14rem]"
      >
        {#each STORAGE_TYPES as st (st)}
          <option value={st}>{STORAGE_LABELS[st]}</option>
        {/each}
      </select>
    </div>

    <div class="flex flex-col gap-1.5">
      <label for="rec-storage-url" class="text-xs text-nyx-muted">
        {connectForm.storageType === 'google_drive' ? t('gui.recover.connect.url_label_gdrive')
          : (connectForm.storageType === 'onedrive' || connectForm.storageType === 'dropbox') ? t('gui.recover.connect.url_label_folder')
          : t('gui.recover.connect.url_label_default')}
      </label>
      <input
        id="rec-storage-url"
        bind:value={connectForm.storageUrl}
        oninput={fieldChanged}
        type="text"
        placeholder={urlPlaceholder[connectForm.storageType] ?? ''}
        class="bg-nyx-surface2 border border-nyx-border rounded-lg px-3 py-2 text-sm
               text-nyx-text placeholder:text-nyx-muted focus:outline-none
               focus:border-nyx-accent transition-colors font-mono text-xs"
      />
    </div>

    <!-- Credentials (SFTP) - matches main editor field-for-field -->
    {#if connectForm.storageType === 'sftp'}
      <div class="flex flex-col gap-2">
        <div class="flex flex-col gap-1">
          <label for="rec-sftp-password" class="text-xs text-nyx-muted">{t('gui.recover.connect.sftp_password')}</label>
          <input
            id="rec-sftp-password"
            bind:value={connectForm.storageSecret}
            oninput={fieldChanged}
            type="password"
            placeholder={t('gui.recover.connect.ph_required')}
            class="bg-nyx-surface2 border border-nyx-border rounded-lg px-3 py-2 text-sm
                   text-nyx-text placeholder:text-nyx-muted font-mono text-xs
                   focus:outline-none focus:border-nyx-accent transition-colors"
          />
          <p class="text-[10px] text-nyx-muted">
            The username + host + port + path live in the URL field above
            (<span class="font-mono">sftp://user@host:port/path</span>).
          </p>
        </div>
      </div>
    {/if}

    <!-- SFTP private key / WebDAV client certificate (mTLS) - optional. -->
    {#if connectForm.storageType === 'sftp' || connectForm.storageType === 'webdav'}
      <div class="flex flex-col gap-1">
        <label for="rec-key-path" class="text-xs text-nyx-muted">
          {connectForm.storageType === 'sftp'
            ? t('gui.recover.connect.mtls_label_sftp')
            : t('gui.recover.connect.mtls_label_webdav')}
        </label>
        <input
          id="rec-key-path"
          bind:value={connectForm.privateKeyPath}
          oninput={fieldChanged}
          type="text"
          placeholder={connectForm.storageType === 'sftp'
            ? t('gui.recover.connect.mtls_ph_sftp')
            : t('gui.recover.connect.mtls_ph_webdav')}
          class="bg-nyx-surface2 border border-nyx-border rounded-lg px-3 py-2 text-sm
                 text-nyx-text placeholder:text-nyx-muted font-mono text-xs
                 focus:outline-none focus:border-nyx-accent transition-colors"
        />
        <p class="text-[10px] text-nyx-muted">
          {connectForm.storageType === 'sftp'
            ? t('gui.recover.connect.mtls_hint_sftp')
            : t('gui.recover.connect.mtls_hint_webdav')}
        </p>
      </div>
    {/if}

    <!-- Credentials & mount path (SMB) - matches main editor -->
    {#if connectForm.storageType === 'smb'}
      <div class="flex flex-col gap-2">
        <div class="flex flex-col gap-1">
          <label for="rec-smb-password" class="text-xs text-nyx-muted">{t('gui.recover.connect.smb_password')}</label>
          <input
            id="rec-smb-password"
            bind:value={connectForm.storageSecret}
            oninput={fieldChanged}
            type="password"
            placeholder={t('gui.recover.connect.ph_required_smb')}
            class="bg-nyx-surface2 border border-nyx-border rounded-lg px-3 py-2 text-sm
                   text-nyx-text placeholder:text-nyx-muted font-mono text-xs
                   focus:outline-none focus:border-nyx-accent transition-colors"
          />
        </div>
        <div class="flex flex-col gap-1">
          <label for="rec-smb-mount" class="text-xs text-nyx-muted">{t('gui.recover.connect.mount_path')}</label>
          <input
            id="rec-smb-mount"
            bind:value={connectForm.storageRegion}
            oninput={fieldChanged}
            type="text"
            placeholder={t('gui.recover.connect.ph_mount')}
            class="bg-nyx-surface2 border border-nyx-border rounded-lg px-3 py-2 text-sm
                   text-nyx-text placeholder:text-nyx-muted font-mono text-xs
                   focus:outline-none focus:border-nyx-accent transition-colors"
          />
          <p class="text-[10px] text-nyx-muted">
            On Linux/macOS, mount the share at the OS level first, then set this
            to the mount point - e.g.
            <span class="font-mono">sudo mount -t cifs //host/share /mnt/smb -o username=USER</span>,
            then <span class="font-mono">/mnt/smb</span>.  On Windows, leave this
            blank; it connects to the UNC path directly.
          </p>
        </div>
      </div>
    {/if}

    <!-- Credentials (WebDAV) - matches main editor -->
    {#if connectForm.storageType === 'webdav'}
      <div class="flex flex-col gap-2">
        <div class="flex flex-col gap-1">
          <label for="rec-webdav-user" class="text-xs text-nyx-muted">{t('gui.recover.connect.webdav_user')}</label>
          <input
            id="rec-webdav-user"
            bind:value={connectForm.storageKeyId}
            oninput={fieldChanged}
            type="text"
            placeholder={t('gui.recover.connect.ph_username')}
            class="bg-nyx-surface2 border border-nyx-border rounded-lg px-3 py-2 text-sm
                   text-nyx-text placeholder:text-nyx-muted font-mono text-xs
                   focus:outline-none focus:border-nyx-accent transition-colors"
          />
        </div>
        <div class="flex flex-col gap-1">
          <label for="rec-webdav-pass" class="text-xs text-nyx-muted">{t('gui.recover.connect.webdav_pass')}</label>
          <input
            id="rec-webdav-pass"
            bind:value={connectForm.storageSecret}
            oninput={fieldChanged}
            type="password"
            placeholder={t('gui.recover.connect.ph_required')}
            class="bg-nyx-surface2 border border-nyx-border rounded-lg px-3 py-2 text-sm
                   text-nyx-text placeholder:text-nyx-muted font-mono text-xs
                   focus:outline-none focus:border-nyx-accent transition-colors"
          />
        </div>
      </div>
    {/if}

    <!-- Credentials (S3 / S3-compat / B2 / GCS / Azure) -->
    {#if connectForm.storageType === 's3' || connectForm.storageType === 's3_compat' || connectForm.storageType === 'azure_blob' || connectForm.storageType === 'backblaze_b2' || connectForm.storageType === 'gcs'}
      <div class="flex flex-col gap-2">
        {#if connectForm.storageType === 's3_compat'}
          <div class="flex flex-col gap-1">
            <label for="rec-endpoint-url" class="text-xs text-nyx-muted">{t('gui.recover.connect.endpoint_url')}</label>
            <input
              id="rec-endpoint-url"
              bind:value={connectForm.storageRegion}
              oninput={fieldChanged}
              type="text"
              placeholder="https://s3.wasabisys.com"
              class="bg-nyx-surface2 border border-nyx-border rounded-lg px-3 py-2 text-sm
                     text-nyx-text placeholder:text-nyx-muted focus:outline-none
                     focus:border-nyx-accent transition-colors font-mono text-xs"
            />
          </div>
          <div class="flex flex-col gap-1">
            <label for="rec-s3-region" class="text-xs text-nyx-muted">{t('gui.recover.connect.s3_region_label')}</label>
            <input
              id="rec-s3-region"
              bind:value={connectForm.s3Region}
              oninput={fieldChanged}
              type="text"
              placeholder={t('gui.recover.connect.s3_region_ph')}
              class="bg-nyx-surface2 border border-nyx-border rounded-lg px-3 py-2 text-sm
                     text-nyx-text placeholder:text-nyx-muted focus:outline-none
                     focus:border-nyx-accent transition-colors font-mono text-xs"
            />
          </div>
        {/if}
        {#if connectForm.storageType === 'gcs'}
          <div class="flex flex-col gap-1">
            <label for="rec-gcs-key" class="text-xs text-nyx-muted">{t('gui.recover.connect.gcs_json')}</label>
            <textarea
              id="rec-gcs-key"
              bind:value={connectForm.storageSecret}
              oninput={fieldChanged}
              rows="6"
              placeholder={t('gui.recover.connect.ph_gcs_json')}
              class="bg-nyx-surface2 border border-nyx-border rounded-lg px-3 py-2 text-sm
                     text-nyx-text placeholder:text-nyx-muted focus:outline-none
                     focus:border-nyx-accent transition-colors font-mono text-xs resize-y"
            ></textarea>
          </div>
        {:else}
          <!-- Azure has no key-id field: the storage account name comes from
               the URL (azure://account/container/...), so only the account
               key (the secret, below) is needed. -->
          {#if connectForm.storageType !== 'azure_blob'}
            <div class="flex flex-col gap-1">
              <label for="rec-key-id" class="text-xs text-nyx-muted">
                {connectForm.storageType === 's3' || connectForm.storageType === 's3_compat'
                  ? t('gui.recover.connect.keyid_access')
                  : t('gui.recover.connect.keyid_application')}
              </label>
              <input
                id="rec-key-id"
                bind:value={connectForm.storageKeyId}
                oninput={fieldChanged}
                type="text"
                placeholder={t('gui.recover.connect.ph_required')}
                class="bg-nyx-surface2 border border-nyx-border rounded-lg px-3 py-2 text-sm
                       text-nyx-text placeholder:text-nyx-muted font-mono text-xs
                       focus:outline-none focus:border-nyx-accent transition-colors"
              />
            </div>
          {/if}
          <div class="flex flex-col gap-1">
            <label for="rec-secret" class="text-xs text-nyx-muted">
              {connectForm.storageType === 's3' || connectForm.storageType === 's3_compat' ? t('gui.recover.connect.secret_s3')
                : connectForm.storageType === 'azure_blob' ? t('gui.recover.connect.secret_azure')
                : t('gui.recover.connect.secret_application')}
            </label>
            <input
              id="rec-secret"
              bind:value={connectForm.storageSecret}
              oninput={fieldChanged}
              type="password"
              placeholder={t('gui.recover.connect.ph_required')}
              class="bg-nyx-surface2 border border-nyx-border rounded-lg px-3 py-2 text-sm
                     text-nyx-text placeholder:text-nyx-muted font-mono text-xs
                     focus:outline-none focus:border-nyx-accent transition-colors"
            />
          </div>
        {/if}
        {#if connectForm.storageType === 's3'}
          <div class="flex flex-col gap-1">
            <label for="rec-region" class="text-xs text-nyx-muted">{t('gui.recover.connect.region')}</label>
            <input
              id="rec-region"
              bind:value={connectForm.storageRegion}
              oninput={fieldChanged}
              type="text"
              placeholder="us-east-1"
              class="bg-nyx-surface2 border border-nyx-border rounded-lg px-3 py-2 text-sm
                     text-nyx-text placeholder:text-nyx-muted focus:outline-none
                     focus:border-nyx-accent transition-colors font-mono text-xs"
            />
          </div>
        {/if}
      </div>
    {/if}

    <!-- OAuth: one-click "Connect with X" button - same UX as
         the main app's BackupSetEditor.  Falls back to the paste-token
         field for users who already have a refresh token from another
         machine or who prefer not to run the browser dance. -->
    {#if connectForm.storageType === 'google_drive' || connectForm.storageType === 'onedrive' || connectForm.storageType === 'dropbox'}
      <div class="flex flex-col gap-2">
        {#if connectForm.storageType === 'dropbox'}
          <button
            onclick={async () => {
              errorMsg = ''
              try {
                const r = await invoke<{ refresh_token: string; email: string }>('rec_dropbox_oauth')
                connectForm.storageSecret = r.refresh_token
                errorMsg = ''
                fieldChanged()
                connMessage = `Connected as ${r.email || '(unknown account)'}.`
              } catch (e) {
                errorMsg = tf('gui.recover.connect.dropbox_oauth_failed', { error: String(e) })
              }
            }}
            class="self-start text-xs px-3 py-1.5 rounded-lg bg-[#0061ff] text-white
                   font-semibold hover:bg-[#0050d0] transition-colors"
          >{t('gui.recover.connect.connect_dropbox')}</button>
        {/if}
        {#if connectForm.storageType === 'google_drive'}
          <button
            onclick={async () => {
              errorMsg = ''
              try {
                const r = await invoke<{ folder_id: string; refresh_token: string; email: string }>(
                  'rec_google_oauth', { folderUrl: connectForm.storageUrl.trim() })
                connectForm.storageUrl = r.folder_id
                connectForm.storageSecret = r.refresh_token
                fieldChanged()
                connMessage = `Connected as ${r.email || '(unknown account)'}.`
              } catch (e) {
                errorMsg = tf('gui.recover.connect.google_oauth_failed', { error: String(e) })
              }
            }}
            class="self-start text-xs px-3 py-1.5 rounded-lg bg-[#4285f4] text-white
                   font-semibold hover:bg-[#357ae8] transition-colors"
          >{t('gui.recover.connect.connect_google')}</button>
        {/if}
        {#if connectForm.storageType === 'onedrive'}
          <div class="flex flex-col gap-1">
            <label for="rec-onedrive-tenant" class="text-xs text-nyx-muted">{t('gui.recover.connect.account_type')}</label>
            <select
              id="rec-onedrive-tenant"
              bind:value={onedriveTenantChoice}
              class="bg-nyx-surface2 border border-nyx-border rounded-lg px-3 py-2 text-sm
                     text-nyx-text focus:outline-none focus:border-nyx-accent transition-colors"
            >
              <option value="common">{t('gui.recover.connect.acct_any')}</option>
              <option value="consumers">{t('gui.recover.connect.acct_personal')}</option>
              <option value="organizations">{t('gui.recover.connect.acct_work')}</option>
              <option value="custom">{t('gui.recover.connect.acct_tenant')}</option>
            </select>
            {#if onedriveTenantChoice === 'custom'}
              <input
                bind:value={onedriveTenantCustom}
                type="text"
                placeholder="00000000-0000-0000-0000-000000000000"
                class="mt-1 bg-nyx-surface2 border border-nyx-border rounded-lg px-3 py-2 text-sm
                       text-nyx-text placeholder:text-nyx-muted font-mono text-xs focus:outline-none
                       focus:border-nyx-accent transition-colors"
              />
            {/if}
          </div>
          <button
            onclick={connectOnedrive}
            disabled={connectingOnedrive}
            class="self-start text-xs px-3 py-1.5 rounded-lg bg-[#0067b8] text-white
                   font-semibold hover:bg-[#005a9e] disabled:opacity-40 transition-colors"
          >{connectingOnedrive ? t('gui.recover.connect.connecting') : t('gui.recover.connect.connect_ms')}</button>
        {/if}

        <!-- Manual (no-local-browser) sign-in relay - works for all 3 providers. -->
        <details class="text-[10px] text-nyx-muted">
          <summary class="cursor-pointer">{t('gui.recover.connect.manual_summary')}</summary>
          <div class="mt-1.5 flex flex-col gap-2">
            {#if connectForm.storageType === 'google_drive'}
              <p class="text-[10px] text-nyx-muted">
                {t('gui.recover.connect.manual_gdrive_hint')}
              </p>
            {/if}
            <button
              type="button"
              onclick={getManualOauthUrl}
              class="self-start text-xs px-3 py-1.5 rounded-lg border border-nyx-border
                     text-nyx-text hover:bg-nyx-surface2 transition-colors"
            >{t('gui.recover.connect.manual_get_link')}</button>
            {#if manualAuthUrl}
              <p class="text-[10px] text-nyx-muted">
                {t('gui.recover.connect.manual_step1')}
              </p>
              <textarea
                readonly
                rows="2"
                value={manualAuthUrl}
                onclick={(e) => (e.currentTarget as HTMLTextAreaElement).select()}
                class="bg-nyx-surface2 border border-nyx-border rounded-lg px-2 py-1.5 text-[10px]
                       text-nyx-text font-mono resize-y"
              ></textarea>
              <p class="text-[10px] text-nyx-muted">
                {t('gui.recover.connect.manual_step2')}
              </p>
              <input
                bind:value={manualPasted}
                type="text"
                placeholder="http://localhost:PORT/?code=...  (or just the code)"
                class="bg-nyx-surface2 border border-nyx-border rounded-lg px-2 py-1.5 text-[10px]
                       text-nyx-text placeholder:text-nyx-muted font-mono focus:outline-none
                       focus:border-nyx-accent transition-colors"
              />
              <button
                type="button"
                onclick={finishManualOauth}
                disabled={!manualPasted.trim() || manualBusy}
                class="self-start text-xs px-3 py-1.5 rounded-lg bg-nyx-accent text-white
                       font-semibold hover:opacity-90 disabled:opacity-40 transition-colors"
              >{manualBusy ? t('gui.recover.connect.manual_finishing') : t('gui.recover.connect.manual_finish')}</button>
            {/if}
          </div>
        </details>

        <details class="text-[10px] text-nyx-muted">
          <summary class="cursor-pointer">{t('gui.recover.connect.paste_token')}</summary>
          <div class="mt-1.5 flex flex-col gap-1">
            <input
              bind:value={connectForm.storageSecret}
              oninput={fieldChanged}
              type="password"
              placeholder={t('gui.recover.connect.ph_refresh_token')}
              class="bg-nyx-surface2 border border-nyx-border rounded-lg px-3 py-2 text-sm
                     text-nyx-text placeholder:text-nyx-muted focus:outline-none
                     focus:border-nyx-accent transition-colors font-mono text-xs"
            />
            <p class="text-[10px] text-nyx-muted">
              {t('gui.recover.connect.paste_token_hint')}
            </p>
          </div>
        </details>
      </div>
    {/if}

    <div class="flex flex-col gap-1">
      <label for="rec-label" class="text-xs text-nyx-muted">{t('gui.recover.connect.label_optional')}</label>
      <input
        id="rec-label"
        bind:value={connectForm.label}
        type="text"
        placeholder={t('gui.recover.connect.ph_label')}
        class="bg-nyx-surface2 border border-nyx-border rounded-lg px-3 py-2 text-sm
               text-nyx-text placeholder:text-nyx-muted focus:outline-none
               focus:border-nyx-accent transition-colors text-xs"
      />
    </div>

    <!-- Test connection (must pass before Continue enables) -->
    <div class="flex items-center gap-3 mt-1">
      <button
        onclick={handleTestConnection}
        disabled={testing}
        class="text-xs px-3 py-1.5 rounded-lg border border-nyx-border text-nyx-muted
               hover:text-nyx-text hover:border-nyx-accent transition-colors
               disabled:opacity-40"
      >
        {testing ? t('gui.recover.connect.testing') : t('gui.recover.connect.test_conn')}
      </button>
      {#if connTested === 'ok'}
        <span class="text-xs text-nyx-success">✓ {connMessage}</span>
      {:else if connTested === 'fail'}
        <span class="text-xs text-nyx-error break-words">✗ {connMessage}</span>
      {:else}
        <span class="text-[10px] text-nyx-muted">{t('gui.recover.connect.required_hint')}</span>
      {/if}
    </div>
  </section>

  <!-- ── Master encryption key (from the source machine) ──────────────────── -->
  <section class="rounded-lg border border-nyx-border bg-nyx-surface p-5 flex flex-col gap-3">
    <span class="text-xs font-medium text-nyx-muted uppercase tracking-wide">
      {t('gui.recover.connect.master_key_title')}
    </span>
    <p class="text-xs text-nyx-muted">
      {@html t('gui.recover.connect.master_key_blurb')}
    </p>
    <div class="flex flex-col gap-1.5">
      <label for="rec-master-key" class="text-xs text-nyx-muted">{t('gui.recover.connect.master_key_label')}</label>
      <textarea
        id="rec-master-key"
        bind:value={connectForm.masterKeyText}
        rows="3"
        placeholder={t('gui.recover.connect.master_key_ph')}
        class="bg-nyx-surface2 border border-nyx-border rounded-lg px-3 py-2 text-sm
               text-nyx-text placeholder:text-nyx-muted focus:outline-none
               focus:border-nyx-accent transition-colors font-mono text-xs resize-none"
      ></textarea>
      <button
        onclick={loadKeyFromFile}
        type="button"
        class="self-start text-xs px-3 py-1.5 rounded-lg border border-nyx-border text-nyx-muted
               hover:text-nyx-text hover:border-nyx-accent transition-colors"
      >{t('gui.recover.unlock.load_file')}</button>
      <p class="text-[10px] text-nyx-muted">
        {@html t('gui.recover.connect.file_hint')}
      </p>
    </div>
  </section>

  {#if errorMsg}
    <p class="text-xs text-nyx-error whitespace-pre-wrap break-words">{errorMsg}</p>
  {/if}

  <button
    onclick={handleContinue}
    disabled={busy || connTested !== 'ok'}
    class="self-end text-sm px-4 py-2 rounded-lg bg-nyx-accent text-nyx-bg font-semibold
           hover:bg-nyx-accent-hi disabled:opacity-40 transition-colors"
    title={connTested !== 'ok' ? t('gui.recover.connect.test_first') : ''}
  >
    {busy ? t('gui.recover.connect.connecting') : t('gui.recover.connect.continue')}
  </button>
</div>
