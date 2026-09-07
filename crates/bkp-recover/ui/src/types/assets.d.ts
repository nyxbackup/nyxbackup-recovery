// Copyright (c) 2026 Nyx Software, LLC
// SPDX-License-Identifier: Apache-2.0
// Nyx Backup Recovery - https://nyxbackup.com

// Image imports are processed by Vite into URL strings.
declare module '*.png' {
  const url: string
  export default url
}
declare module '*.svg' {
  const url: string
  export default url
}
