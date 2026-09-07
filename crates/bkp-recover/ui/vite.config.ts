// Copyright (c) 2026 Nyx Software, LLC
// SPDX-License-Identifier: Apache-2.0
// Nyx Backup Recovery - https://nyxbackup.com

import { defineConfig } from 'vite'
import { svelte } from '@sveltejs/vite-plugin-svelte'

const localesDir = new URL('../../../locales', import.meta.url).pathname
const workspaceRoot = new URL('../../..', import.meta.url).pathname

export default defineConfig({
  plugins: [svelte()],
  clearScreen: false,
  resolve: {
    alias: {
      '@locales': localesDir,
    },
  },
  server: {
    port: 5174,
    strictPort: true,
    fs: { allow: [workspaceRoot] },
  },
  envPrefix: ['VITE_', 'TAURI_'],
  build: {
    target: 'chrome105',
    minify: !process.env.TAURI_DEBUG,
    sourcemap: !!process.env.TAURI_DEBUG,
    chunkSizeWarningLimit: 2000,
  },
})
