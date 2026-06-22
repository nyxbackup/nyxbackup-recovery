#!/usr/bin/env python3
# Copyright (c) 2026 Nyx Software, LLC
# SPDX-License-Identifier: Apache-2.0
# Nyx Backup Recovery - https://nyxbackup.com

"""
Fill every untranslated locale string from English in one pass.

For each non-English locale, any key that is missing or still equal to the
English source is machine-translated via the free Google Translate endpoint
(no API key required) and written back.  Placeholders, HTML, and brand /
literal tokens are masked so the translator cannot corrupt them; if a mask
does not survive the round trip the string is left in English rather than
written broken.

This replaces the older export -> translate -> import CSV pipeline: it detects
the untranslated keys itself, translates them, and merges them, so the whole
fill is a single command.  Run `opus_review.py` afterwards for the
context-aware quality pass that fixes wrong-sense / wrong-register strings.

Usage:
  python3 scripts/i18n/translate_fill.py                 # all non-en locales
  python3 scripts/i18n/translate_fill.py --locale fr     # one locale
  python3 scripts/i18n/translate_fill.py --dry-run       # report, do not write
"""

import argparse
import json
import os
import re
import sys
import time
import urllib.parse
import urllib.request

LOCALE_DIR = os.path.join(os.path.dirname(__file__), "..", "..", "locales")

LANGS = ['cs', 'da', 'de', 'el', 'es', 'fi', 'fr', 'hi', 'hu', 'it', 'ja',
         'ko', 'nb', 'nl', 'pl', 'pt', 'ro', 'ru', 'sv', 'tr', 'uk', 'vi', 'zh']

# Literal substrings that must survive untranslated (longest first so a
# specific match wins over a shorter prefix).
LITERALS = [
    '~/.local/share/nyxbackup-recover/logs/recovery.log',
    'KEY=abcd1234...',
    'Nyx Backup Recovery',
    'RateLimitedBackend',
    'Nyx Software, LLC',
    'Nyx Backup',
    'Apache-2.0',
    'Mode B',
    'KEY=',
    '(c)',
    'Mbps', 'OAuth', 'WebDAV', 'SFTP', 'GCS', 'SMB', 'JSON',
    '✓',  # check mark
]

MASKABLE = [
    re.compile(r'<code[^>]*>.*?</code>', re.S),   # whole literal code blocks
    re.compile(r'<span[^>]*>.*?</span>', re.S),   # whole literal span blocks
    re.compile(r'<[^>]+>'),                        # any stray tag
    re.compile(r'\{[^}]+\}'),                      # {placeholders}
    re.compile(r'&\w+;'),                          # &gt; &lt; &amp; ...
]


def mask(text):
    """Replace protected fragments with #i# markers; return (masked, parts)."""
    parts = []

    def stash(m):
        parts.append(m.group(0))
        return f'#{len(parts) - 1}#'

    for pat in MASKABLE:
        text = pat.sub(stash, text)
    for lit in LITERALS:
        if lit in text:
            idx = len(parts)
            parts.append(lit)
            text = text.replace(lit, f'#{idx}#')
    return text, parts


def unmask(text, parts):
    """Restore #i# markers (tolerant of spaces the translator may insert)."""
    return re.sub(r'#\s*(\d+)\s*#', lambda m: parts[int(m.group(1))], text)


def gt(q, tl, tries=4):
    url = ('https://translate.googleapis.com/translate_a/single'
           '?client=gtx&sl=en&dt=t&tl=' + tl + '&q=' + urllib.parse.quote(q))
    last = None
    for attempt in range(tries):
        try:
            with urllib.request.urlopen(url, timeout=20) as r:
                data = json.load(r)
            return ''.join(seg[0] for seg in data[0] if seg[0])
        except Exception as e:  # noqa: BLE001 - best-effort, retry then skip
            last = e
            time.sleep(0.6 * (attempt + 1))
    raise last


def translate_one(src, tl):
    """Translate `src` into `tl`, preserving masks.  Returns None to keep EN."""
    masked, parts = mask(src)
    try:
        out = gt(masked, tl)
    except Exception as e:  # noqa: BLE001
        print(f'  request failed, keep EN: {e}', file=sys.stderr)
        return None
    found = {int(m) for m in re.findall(r'#\s*(\d+)\s*#', out)}
    if found != set(range(len(parts))):
        return None  # a mask was lost - do not trust the result
    restored = unmask(out, parts).strip()
    return restored or None


def load(locale):
    return json.load(open(os.path.join(LOCALE_DIR, f'{locale}.json')))


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument('--locale', help='only this locale (default: all non-en)')
    ap.add_argument('--dry-run', action='store_true', help='report, do not write')
    args = ap.parse_args()

    en = load('en')
    targets = [args.locale] if args.locale else LANGS
    for tl in targets:
        if tl == 'en' or tl not in LANGS:
            continue
        data = load(tl)
        todo = [k for k in en if data.get(k) is None or data.get(k) == en[k]]
        filled = kept = 0
        for k in todo:
            res = translate_one(en[k], tl) if not args.dry_run else None
            if res and res != en[k]:
                data[k] = res
                filled += 1
            else:
                kept += 1
            if not args.dry_run:
                time.sleep(0.12)
        if not args.dry_run:
            ordered = {k: data.get(k, en[k]) for k in en}
            with open(os.path.join(LOCALE_DIR, f'{tl}.json'), 'w') as f:
                json.dump(ordered, f, ensure_ascii=False, indent=2)
                f.write('\n')
        print(f'[{tl}] untranslated={len(todo)} filled={filled} kept-EN={kept}')


if __name__ == '__main__':
    main()
