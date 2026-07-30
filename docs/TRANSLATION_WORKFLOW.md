# Translation Workflow

How to keep the 23 non-English locales in sync with `locales/en.json`.

The flow is two steps: **fill** (machine-translate anything still in English),
then an optional **review** (a context-aware quality pass using an LLM).
Both are idempotent and safe to re-run.

```
locales/en.json  --(translate_fill.py)-->  locales/*.json  --(opus_review.py)-->  locales/*.json
```

---

## Step 1 - Fill missing translations

When new `gui.recover.*` keys land in `locales/en.json`, run:

```bash
python3 scripts/i18n/translate_fill.py            # all non-en locales
python3 scripts/i18n/translate_fill.py --locale fr  # one locale
python3 scripts/i18n/translate_fill.py --dry-run    # report, do not write
```

For every locale, any key that is missing or still equal to the English
source is machine-translated via the **free** Google Translate endpoint - no
API key, no Google Cloud account. The script detects, translates, and merges
in one pass (it replaces the old export -> translate -> import CSV pipeline).

**Mask safety.** Before translating, the script masks anything the translator
must not touch and restores it afterwards:

- `{placeholders}` (`{path}`, `{count}`, `{error}`, ...)
- HTML (`<code>...</code>`, `<span>...</span>`, entities like `&gt;`)
- brand / literal tokens (`Nyx Backup`, `KEY=`, `Apache-2.0`, file paths,
  `Mode B`, `OAuth`, `SFTP`, the check mark, ...)

If a mask does not survive the round trip, that string is left in English
rather than written broken. Extend the `LITERALS` / `MASKABLE` lists at the
top of the script when a new placeholder or brand name appears.

Commit the result:

```bash
git add locales/*.json && git commit -m "i18n: refresh non-English locales"
```

---

## Step 2 (optional) - LLM context-aware review

Machine translation is grammatically correct but occasionally picks the wrong
word *sense* for short UI strings - "Back" as a body part instead of
navigation, "Set" as a verb instead of the backup-set noun, "Master key" as a
physical skeleton key instead of the crypto key. `opus_review.py` reads each
string plus optional per-key context and rewrites only the cells where the
sense or register is wrong.

```bash
python3 scripts/i18n/opus_review.py                 # all locales
python3 scripts/i18n/opus_review.py --context-only   # only keys with context
python3 scripts/i18n/opus_review.py --locale fr      # one locale
python3 scripts/i18n/opus_review.py --dry-run        # preview, no writes
```

### Prerequisites

```bash
python3 -m venv .venv-i18n
.venv-i18n/bin/pip install anthropic
export ANTHROPIC_API_KEY=sk-ant-...
# then run with: .venv-i18n/bin/python scripts/i18n/opus_review.py
```

### Cost

`opus_review.py` calls the Anthropic API, billed pay-as-you-go per token from
prepaid credit (which starts at zero and is separate from any chat
subscription). For a hard cap, buy a small prepaid amount (e.g. $5) at
console.anthropic.com and **leave auto-recharge off** - calls fail with a
no-credit error rather than overshooting. A `--context-only` pass across all
locales costs well under a dollar; a full pass is ~$10-15.

### Per-key context: `locales/en_context.json`

For keys whose bare English is ambiguous, add an entry so the reviewer knows
the intended sense:

```json
{
  "gui.recover.browse.set_n": {
    "type": "label",
    "context": "Labels a numbered backup set (a noun, e.g. 'Set 1'). NOT the verb 'to set/configure'."
  }
}
```

`type` is the UI element kind (button, label, prose, error-title, ...);
`context` is one or two sentences on the meaning and the senses to avoid. Add
entries reactively - when you spot a bad rendering in the running UI, or when
you add a single-word button verb or a product noun (`pack`, `snapshot`,
`manifest`, `chunk`). The pass is idempotent: it only rewrites cells whose
translation actually changes.

---

## Adding a language

1. Add `locales/<code>.json` (copy `en.json`, then run `translate_fill.py
   --locale <code>`).
2. Import it in `crates/bkp-recover/ui/src/lib/i18n.svelte.ts` and add the
   code to the `SUPPORTED` list there.
3. Add the code to `LANGS` in `scripts/i18n/translate_fill.py`.
4. Add the endonym to the `LANGUAGES` picker in
   `crates/bkp-recover/ui/src/views/Settings.svelte`.

The English locale (`en.json`) is imported statically and always shipped;
other languages lazy-load on demand from the webview bundle.

---

## Quality status

Translations are machine-generated (free Google endpoint) with an LLM
sense/register review on the ambiguous short strings. That clears the worst
wrong-sense bugs and is good enough for a native speaker to navigate the UI,
but it is **not** native-speaker quality - long help-text prose in particular
is unreviewed. For launch-priority locales (ES, FR, DE, JA, ZH, PT) a paid
native-speaker QA pass on top of this flow is worthwhile.

---

## Reference

- `scripts/i18n/translate_fill.py` - detect + machine-translate + merge (free endpoint).
- `scripts/i18n/opus_review.py` - LLM context-aware review pass (Anthropic API).
- `locales/<code>.json` - one file per language; `locales/en_context.json` - optional review hints.
- Runtime: `crates/bkp-recover/ui/src/lib/i18n.svelte.ts` exposes `t(key)` and
  `tf(key, vars)`; the locale is resolved at startup from saved settings ->
  `navigator.language` -> English.
