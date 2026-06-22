// Copyright (c) 2026 Nyx Software, LLC
// SPDX-License-Identifier: Apache-2.0
// Nyx Backup Recovery - https://nyxbackup.com

import { invoke } from '@tauri-apps/api/core'

/// Connect-screen args.  `label` is optional; the daemon falls back to a
/// `<type>: <url>` summary when omitted.
export interface ConnectArgs {
  endpoint_type: string
  url: string
  key_id: string
  secret: string
  label?: string
  region?: string
  endpoint_url?: string
  host?: string
  port?: number
  username?: string
  /** SFTP private-key file path, or WebDAV client-certificate PEM path. */
  private_key_path?: string
}

export interface ConnectReply { phase: string; label: string }

export interface RecentEndpoint {
  endpoint_type: string
  url: string
  key_id: string
  secret: string
  region: string
  endpoint_url: string
  label: string
  last_used: number
}

export interface SnapshotSummary {
  snapshot_id: string
  set_id: string
  created_at: number
  files_total: number
  bytes_total: number
  /// Empty for older snapshots; UI falls back to
  /// hostname, then "Set N".
  set_name: string
  hostname: string
}

export interface DestFreeSpace {
  free_bytes:   number
  total_bytes:  number
  determinable: boolean
}

export interface Settings {
  download_bandwidth_kbps: number
  log_level: string
  theme: string
  /// "auto" follows OS locale; otherwise one of the 24 supported codes.
  locale: string
  /// Sparse restore: punch all-zero regions as filesystem holes (default true).
  restore_sparse: boolean
}

export interface AppInfo {
  name: string
  version: string
  target: string
}

export const api = {
  connect:        (args: ConnectArgs): Promise<ConnectReply> => invoke('rec_connect', { args }),
  disconnect:     (): Promise<void> => invoke('rec_disconnect'),
  unlock:         (masterKeyHex: string): Promise<{ phase: string }> =>
                    invoke('rec_unlock', { args: { master_key_hex: masterKeyHex } }),
  listSnapshots:  (): Promise<SnapshotSummary[]> => invoke('rec_list_snapshots'),
  pauseRestore:   (): Promise<void> => invoke('rec_pause_restore'),
  resumeRestore:  (): Promise<void> => invoke('rec_resume_restore'),
  cancelRestore:  (): Promise<void> => invoke('rec_cancel_restore'),
  openFolder:     (path: string): Promise<void> => invoke('rec_open_folder', { path }),
  localDesktop:   (): Promise<string> => invoke('rec_local_desktop'),
  getFreeSpace:   (path: string): Promise<DestFreeSpace> => invoke('rec_get_free_space', { path }),
  getRecent:      (): Promise<RecentEndpoint[]> => invoke('rec_get_recent'),
  removeRecent:   (endpointType: string, url: string, keyId: string): Promise<RecentEndpoint[]> =>
                    invoke('rec_remove_recent', { endpointType, url, keyId }),
  getSettings:    (): Promise<Settings> => invoke('rec_get_settings'),
  saveSettings:   (s: Settings): Promise<void> => invoke('rec_save_settings', { settings: s }),
  appInfo:        (): Promise<AppInfo> => invoke('rec_app_info'),
}
