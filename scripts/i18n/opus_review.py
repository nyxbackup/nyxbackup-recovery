#!/usr/bin/env python3
# Copyright (c) 2026 Nyx Software, LLC
# SPDX-License-Identifier: Apache-2.0
# Nyx Backup Recovery - https://nyxbackup.com

"""
Run Claude Opus as a *reviewer* over the existing Google-Translated
locale files.  For each (locale, key, en_source, current_value, context),
Opus either:
  - echoes `current_value` unchanged when the tone is fine, OR
  - rewrites it to product-UI register for that locale, preserving
    every `{placeholder}`, `[F3]` shortcut, and brand name.

Each entry is augmented with per-key context from `locales/en_context.json`
when present - the UI element type (button, prose, error-suggestion, ...)
and a one-line note on what the string means in the running UI.  That
context is the signal that prevents wrong-sense translations like the
French "Dos" -> "Retour" / Chinese 输入 -> 导入 issues that pure
Google Translate v2 can't catch.

Surgical quality lift on the strings that need it; leaves the bulk
untouched.  Cheaper than a full retranslate (~$10-15 vs $100).

Prerequisites:

  # On Debian/Ubuntu (PEP 668 forbids system-wide pip), use a venv:
  python3 -m venv .venv-i18n
  .venv-i18n/bin/pip install anthropic
  # Then run with:
  #   .venv-i18n/bin/python scripts/i18n/opus_review.py
  #
  # Other distros without the externally-managed-environment lock:
  pip install anthropic

  export ANTHROPIC_API_KEY=sk-ant-...

Usage:
  python3 scripts/i18n/opus_review.py                # all 23 non-en locales
  python3 scripts/i18n/opus_review.py --locale it    # one locale
  python3 scripts/i18n/opus_review.py --dry-run      # don't write JSON
  python3 scripts/i18n/opus_review.py --context-only # only review keys that
                                                     # have an en_context.json
                                                     # entry (cheap focused pass)

Cost ballpark (default config):
  - Model: claude-opus-4-8
  - Adaptive thinking: enabled (budget 4k tokens)
  - ~880 keys × 23 locales = 20k decisions
  - Batched 40 keys per request -> ~510 requests
  - Estimated: $10-20 total at current Anthropic pricing
  - --context-only narrows to ~20 keys × 23 locales = ~500 decisions,
    estimated under $1 per pass.
"""

import argparse
import json
import os
import sys
from typing import Any

try:
    import anthropic  # type: ignore
except ImportError:
    sys.exit(
        "Missing dependency.  Install with:  pip install anthropic"
    )

LOCALE_DIR = os.path.join(os.path.dirname(__file__), "..", "..", "locales")
NON_EN = [
    "cs", "da", "de", "el", "es", "fi", "fr", "hi", "hu", "it",
    "ja", "ko", "nb", "nl", "pl", "pt", "ro", "ru", "sv", "tr",
    "uk", "vi", "zh",
]

LOCALE_FULLNAME = {
    "cs": "Czech", "da": "Danish", "de": "German", "el": "Greek",
    "es": "Spanish", "fi": "Finnish", "fr": "French", "hi": "Hindi",
    "hu": "Hungarian", "it": "Italian", "ja": "Japanese", "ko": "Korean",
    "nb": "Norwegian Bokmål", "nl": "Dutch", "pl": "Polish",
    "pt": "Portuguese", "ro": "Romanian", "ru": "Russian",
    "sv": "Swedish", "tr": "Turkish", "uk": "Ukrainian",
    "vi": "Vietnamese", "zh": "Simplified Chinese",
}

# Tokens Opus MUST echo verbatim.  Same set used by translate_fill.py.
DO_NOT_TRANSLATE = (
    "Nyx Backup, Glacier Deep Archive, Deep Archive, Glacier, "
    "Google Drive, Google Cloud Storage, Microsoft OneDrive, OneDrive, "
    "Dropbox, Backblaze B2, Backblaze, Amazon S3, S3-Compatible, "
    "S3-compat, Azure Blob Storage, Azure, SFTP, SMB, CIFS, GCS, "
    "OAuth, UAC, JSON, HTTP, HTTPS, SSH, URL, UNC, SSO, WSL, MSI"
)

SYSTEM_PROMPT = f"""You are a senior product-UI localisation reviewer for a desktop backup
application called Nyx Backup.  Your task is to review translations
produced by Google Translate and tighten them where the tone is wrong,
the phrasing is awkward, a placeholder was mishandled, a brand name
was translated when it should not have been, or - most commonly - the
WRONG WORD SENSE was picked because Google Translate had no context.

Each entry may include a `context` field describing how the string is
used in the running UI (UI element type, what action triggers it, what
follows, word senses to avoid).  When context is present, USE IT - it
is the primary signal for word-sense disambiguation.

Hard rules - violating any of these is worse than leaving the
translation imperfect:

1. Preserve every `{{placeholder}}` token EXACTLY.  Examples: `{{name}}`,
   `{{ms}}`, `{{provider}}`, `{{version}}`, `{{tier}}`, `{{count}}`.
   Never translate, reorder, or re-wrap them.
2. Preserve every `[F3]`-style keyboard shortcut EXACTLY.  Examples:
   `[F3]`, `[Tab]`, `[Esc]`, `[Enter]`, `[BackTab]`.
3. These brand / proper names stay verbatim: {DO_NOT_TRANSLATE}.
4. Match the tone of a confident, concise product-UI writer.  No
   over-formality, no padding words, no literal back-translation.
5. If the Google translation is already correct AND context is silent,
   ECHO IT UNCHANGED.  Do not rewrite to show effort.
6. If context is present and the current translation conflicts with it
   (e.g. context says "button verb 'to import data'" but current uses
   "to enter/type text"), REWRITE to match the context's word sense.

Output a SINGLE JSON object on the form
  {{"reviewed": {{"<key>": "<final value>", ...}}}}
- One entry per input key.
- `final value` is either the input value verbatim, or your improved version.
- No prose, no markdown, no comments.  JSON only.
"""

BATCH_SIZE = 40


def load(code: str) -> dict[str, Any]:
    with open(os.path.join(LOCALE_DIR, f"{code}.json"), encoding="utf-8") as f:
        return json.load(f)


def load_context() -> dict[str, dict[str, Any]]:
    """Load locales/en_context.json if present.  Strips the `_meta` entry.
    Returns an empty dict when the file is absent so the script keeps
    working as a context-free reviewer for users who haven't built the
    companion yet."""
    path = os.path.join(LOCALE_DIR, "en_context.json")
    if not os.path.exists(path):
        return {}
    with open(path, encoding="utf-8") as f:
        raw = json.load(f)
    return {k: v for k, v in raw.items() if not k.startswith("_")}


def review_batch(
    client: anthropic.Anthropic,
    target_lang: str,
    en_map: dict[str, str],
    current_map: dict[str, str],
    context_map: dict[str, dict[str, Any]],
) -> dict[str, str]:
    entries = []
    for k in current_map.keys():
        item: dict[str, Any] = {
            "key": k,
            "en": en_map[k],
            "current": current_map.get(k, ""),
        }
        if k in context_map:
            ctx = context_map[k]
            # Only forward the fields the model needs - skip schema cruft.
            keep = {f: ctx[f] for f in ("type", "context", "do_not_translate") if f in ctx}
            if keep:
                item["context"] = keep
        entries.append(item)
    user_msg = json.dumps({
        "target_language": target_lang,
        "entries": entries,
    }, ensure_ascii=False)

    resp = client.messages.create(
        model="claude-opus-4-8",
        max_tokens=8000,
        system=SYSTEM_PROMPT,
        messages=[{"role": "user", "content": user_msg}],
        # Adaptive thinking helps a lot for short-but-tricky strings where
        # the literal Google translation is grammatically fine but tonally
        # wrong; the thinking step produces the "echo unchanged" judgement
        # instead of unnecessary rewrites.  Opus 4.8 uses the
        # `thinking.type=adaptive` shape with `output_config.effort` to
        # control how much the model thinks; "low" is appropriate for short
        # per-key reviews and keeps the per-batch cost down.
        thinking={"type": "adaptive"},
        extra_body={"output_config": {"effort": "low"}},
    )
    # Concatenate any text blocks (Opus may emit a thinking block first).
    text = ""
    for block in resp.content:
        if getattr(block, "type", "") == "text":
            text += getattr(block, "text", "")
    text = text.strip()
    # Find the first '{' and the matching last '}'.
    start, end = text.find("{"), text.rfind("}")
    if start == -1 or end == -1:
        raise RuntimeError(f"Opus did not return JSON:\n{text[:300]}")
    parsed = json.loads(text[start:end + 1])
    return parsed.get("reviewed", {})


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--locale", help="Limit to one locale code")
    ap.add_argument("--dry-run", action="store_true",
                    help="Don't write the JSON files back")
    ap.add_argument("--context-only", action="store_true",
                    help="Only review keys that have an en_context.json entry "
                         "(cheap focused pass; estimated < $1 per full run)")
    ap.add_argument("--only-keys",
                    help="Review ONLY these keys (delta polish after adding new "
                         "strings): a comma-separated list, or @path to a file "
                         "with one key per line.  Avoids re-billing the whole "
                         "corpus when only a handful of keys changed.")
    args = ap.parse_args()

    only_keys: set[str] | None = None
    if args.only_keys:
        if args.only_keys.startswith("@"):
            with open(args.only_keys[1:], encoding="utf-8") as fh:
                only_keys = {ln.strip() for ln in fh if ln.strip()}
        else:
            only_keys = {k.strip() for k in args.only_keys.split(",") if k.strip()}
        print(f"Delta mode: reviewing only {len(only_keys)} key(s) per locale")

    if "ANTHROPIC_API_KEY" not in os.environ:
        sys.exit(
            "Missing ANTHROPIC_API_KEY env var.\n"
            "Set it from your Anthropic Console -> API Keys.\n"
            "  export ANTHROPIC_API_KEY=sk-ant-..."
        )

    client = anthropic.Anthropic()
    en = load("en")
    context_map = load_context()
    if context_map:
        print(f"Loaded en_context.json: {len(context_map)} key(s) with context")
    elif args.context_only:
        sys.exit(
            "--context-only requested but locales/en_context.json is empty / "
            "missing.  Build the context file first or drop the flag."
        )
    targets = [args.locale] if args.locale else NON_EN

    for code in targets:
        if code not in LOCALE_FULLNAME:
            print(f"Skipping unknown locale: {code}")
            continue
        current = load(code)
        # Only review keys that exist in BOTH en and current.
        keys = [k for k in en.keys() if k in current]
        if args.context_only:
            keys = [k for k in keys if k in context_map]
        if only_keys is not None:
            keys = [k for k in keys if k in only_keys]
        print(f"\n=== {LOCALE_FULLNAME[code]} ({code}): {len(keys)} keys ===")
        improvements = 0
        for i in range(0, len(keys), BATCH_SIZE):
            batch_keys = keys[i : i + BATCH_SIZE]
            en_map = {k: en[k] for k in batch_keys}
            current_map = {k: current[k] for k in batch_keys}
            try:
                reviewed = review_batch(
                    client, LOCALE_FULLNAME[code], en_map, current_map, context_map,
                )
            except Exception as e:
                print(f"  batch {i}-{i+len(batch_keys)} failed: {e}")
                continue
            for k, v in reviewed.items():
                if k in current and isinstance(v, str) and v != current[k]:
                    current[k] = v
                    improvements += 1
            print(f"  ...reviewed {min(i + BATCH_SIZE, len(keys))} / {len(keys)}")

        print(f"  {improvements} improvement(s){' (dry-run)' if args.dry_run else ''}")
        if not args.dry_run and improvements > 0:
            with open(os.path.join(LOCALE_DIR, f"{code}.json"), "w", encoding="utf-8") as f:
                json.dump(current, f, ensure_ascii=False, indent=2)
                f.write("\n")

    return 0


if __name__ == "__main__":
    sys.exit(main())
