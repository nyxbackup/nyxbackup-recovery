// Copyright (c) 2026 Nyx Software, LLC
// SPDX-License-Identifier: Apache-2.0
// Nyx Backup Recovery - https://nyxbackup.com

/** @type {import('tailwindcss').Config} */
export default {
  content: ['./src/**/*.{html,js,ts,svelte}', './index.html'],
  theme: {
    extend: {
      colors: {
        nyx: {
          bg:          'rgb(var(--nyx-bg) / <alpha-value>)',
          surface:     'rgb(var(--nyx-surface) / <alpha-value>)',
          surface2:    'rgb(var(--nyx-surface2) / <alpha-value>)',
          border:      'rgb(var(--nyx-border) / <alpha-value>)',
          accent:      'rgb(var(--nyx-accent) / <alpha-value>)',
          'accent-hi': 'rgb(var(--nyx-accent-hi) / <alpha-value>)',
          success:     'rgb(var(--nyx-success) / <alpha-value>)',
          warning:     'rgb(var(--nyx-warning) / <alpha-value>)',
          error:       'rgb(var(--nyx-error) / <alpha-value>)',
          text:        'rgb(var(--nyx-text) / <alpha-value>)',
          muted:       'rgb(var(--nyx-muted) / <alpha-value>)',
        },
      },
    },
  },
  plugins: [],
}
