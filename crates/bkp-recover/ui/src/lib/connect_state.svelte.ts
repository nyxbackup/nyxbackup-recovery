// Copyright (c) 2026 Nyx Software, LLC
// SPDX-License-Identifier: Apache-2.0
// Nyx Backup Recovery - https://nyxbackup.com

// Singleton-scoped form state for the Connect screen.  Lifted out of the
// component so that navigating Back from the Browse / About / Settings
// screens doesn't lose the storage credentials and the pasted master key
// (re-typing all of that on every back-press is a real-world annoyance).  Module-level $state lives for the
// process lifetime; cleared explicitly on Disconnect.

export type EndpointType =
  | 'local'
  | 's3' | 's3_compat' | 'azure_blob' | 'backblaze_b2'
  | 'gcs' | 'sftp' | 'smb' | 'webdav'
  | 'google_drive' | 'onedrive' | 'dropbox'

export const connectForm = $state<{
  storageType:   EndpointType
  storageUrl:    string
  storageKeyId:  string
  storageSecret: string
  storageRegion: string
  endpointUrl:   string
  s3Region:      string
  host:          string
  port:          number
  privateKeyPath: string
  label:         string
  masterKeyText: string
}>({
  storageType:   's3',
  storageUrl:    '',
  storageKeyId:  '',
  storageSecret: '',
  storageRegion: '',
  endpointUrl:   '',
  s3Region:      '',
  host:          '',
  port:          22,
  privateKeyPath: '',
  label:         '',
  masterKeyText: '',
})

export function clearConnectForm() {
  connectForm.storageType   = 's3'
  connectForm.storageUrl    = ''
  connectForm.storageKeyId  = ''
  connectForm.storageSecret = ''
  connectForm.storageRegion = ''
  connectForm.endpointUrl   = ''
  connectForm.s3Region      = ''
  connectForm.host          = ''
  connectForm.port          = 22
  connectForm.privateKeyPath = ''
  connectForm.label         = ''
  connectForm.masterKeyText = ''
}
