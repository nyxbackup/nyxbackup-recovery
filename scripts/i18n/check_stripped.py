#!/usr/bin/env python3
# Copyright (c) 2026 Nyx Software, LLC. All rights reserved.
# Nyx Backup - https://nyxbackup.com
"""Find locale strings that were saved accent-stripped or transliterated.

Several in-session translation batches (April - November 2026) wrote their
strings in ASCII only: de "Vollstandigen" with its umlaut dropped, el
"Kleisimo" (the Greek word for Close in Latin letters), pl "blad" missing
its stroked l and ogonek.  Roughly 800 strings across 23 locales
were affected before this check existed.  The cause was the ASCII-only rule
for SOURCE files being applied to translation text typed into a merge
script.  Translations must use each language's own script and diacritics.

A value is reported when it is pure ASCII, differs from English, and either
  (a) the locale is written in a non-Latin script and the value contains
      ordinary lower-case words, or
  (b) it is pure ASCII and contains a word that never appears in the
      locale's accented strings but whose accent-stripped form matches a
      word that does (German ae/oe/ue/ss spellings included);
  (b') with --loose only: any string contains a word (4+ letters, no
      accents) whose accented spelling appears in the same file at least
      twice as often.  This catches strings stripped only in part, but it
      also lists real unaccented words (vi "ngay", cs "souboru", da
      "Nogle"), so its output is for a person to review, not a gate.
  (c) a word mixes Latin letters with Cyrillic or Greek ones - a Latin "i"
      typed for the Ukrainian one looks identical on screen but breaks
      search, sorting and spell-checking (39 such words in uk in 2026).
(b) and (b') find only words the same file spells correctly elsewhere, so
a clean run is evidence, not proof.

Usage:  python3 scripts/i18n/check_stripped.py [--loose] [repo-root]
Exit status 1 when the default checks report anything; --loose candidates
never change the exit status.

Standard library only; this file
is ASCII, so every non-ASCII character below is written as an escape.
"""
import json
import re
import sys
import unicodedata
from collections import Counter, defaultdict
from pathlib import Path

LOCALES = "es cs da de el fi fr hi hu it ja ko nb nl pl pt ro ru sv tr uk vi zh".split()
NON_LATIN = {"el", "hi", "ja", "ko", "zh", "ru", "uk"}
# Non-Latin scripts that mark stress or vowels with combining accents, so a
# stripped word is still detectable: Greek tonos ("fakelo" without its accent).
ACCENTED_NON_LATIN = {"el"}

# Strings reviewed and confirmed correct although a rule matches them.  A bare
# key applies to every locale; "code:key" to one.
ALLOW = {
    # URL and path examples whose words are placeholders, not prose.
    "gui.wizard.storage_url_s3",
    "gui.recover.connect.mtls_ph_sftp",
    # Recovery Tool: ro "Vezi sursa" (the source) is the definite form.
    "ro:gui.recover.about.source",
    # ro "Ora zilei" (time of day): "ora" is the correct definite form.
    "ro:gui.onboarding.sched_time_label",
    # pl "Komponenty open source i licencje": "licencje" is the correct plural.
    "pl:gui.about.notices_title",
    # pl "Najnowsza: {last}" (latest): the nominative is right for a label.
    "pl:gui.snapshot.no_change_last",
    # Nyx Mount: fr "Ajoute nyx_mnt_cli" (adds) and pl "Synchronizacja"
    # (nominative label) are the right forms, not stripped ones.
    "fr:gui.settings.cli_help",
    "pl:gui.tray.paused",
    "pl:gui.tray.state_syncing",
    "pl:gui.dashboard.syncing",
}

# Words that are correct without an accent although an accented form exists:
# es "min" (minutes) next to "min." with an accent (minimum); es "este"
# (this) next to "esté" (subjunctive of estar).
WORD_ALLOW = {"es": {"min", "este"}}

# Letters NFKD does not decompose, mapped to their usual ASCII stand-ins.
FOLD = {
    "\u0131": "i",   # dotless i
    "\u0142": "l",   # l with stroke
    "\u0111": "d",   # d with stroke
    "\u00f8": "o",   # o with stroke
    "\u00e6": "ae",  # ae ligature
    "\u00df": "ss",  # sharp s
}
GERMAN = {
    "\u00e4": "ae", "\u00f6": "oe", "\u00fc": "ue",
    "\u00c4": "Ae", "\u00d6": "Oe", "\u00dc": "Ue", "\u00df": "ss",
}


LATIN = re.compile(r"[A-Za-z]")
OTHER_SCRIPT = re.compile("[\u0370-\u03ff\u0400-\u04ff]")


def strip_accents(word):
    out = "".join(c for c in unicodedata.normalize("NFKD", word) if not unicodedata.combining(c))
    for k, v in FOLD.items():
        out = out.replace(k, v)
    return out


def german_spelling(word):
    for k, v in GERMAN.items():
        word = word.replace(k, v)
    return word


def words(value):
    # Turkish dotted capital I lower-cases to "i" plus a combining dot, which
    # would make every word starting with it look like an accented spelling.
    value = value.replace("\u0130", "I")
    value = re.sub(r"\{[^}]*\}|https?://\S+|\S+@\S+|`[^`]*`", " ", value)
    return re.findall(r"[^\W\d_]+", value)


def is_ascii(value):
    return all(ord(c) < 128 for c in value)


def check(root, loose=False):
    en = json.loads((root / "locales" / "en.json").read_text(encoding="utf-8"))
    found = []
    candidates = []
    for code in LOCALES:
        data = json.loads((root / "locales" / f"{code}.json").read_text(encoding="utf-8"))
        counts = Counter(w.lower() for value in data.values() for w in words(value))
        trusted = {w.lower() for value in data.values() if not is_ascii(value) for w in words(value)}
        accented = defaultdict(Counter)
        for w, n in counts.items():
            if strip_accents(w) != w:
                accented[strip_accents(w)][w] += n
                if code == "de":
                    accented[german_spelling(w)][w] += n
        for key, value in data.items():
            mixed = [w for w in words(value) if LATIN.search(w) and OTHER_SCRIPT.search(w)]
            if mixed:
                found.append((code, key, value, [f"mixed script: {w}" for w in mixed[:4]]))
            if key in ALLOW or f"{code}:{key}" in ALLOW or value == en.get(key):
                continue
            ws = words(value)
            if code in NON_LATIN:
                hits = [w for w in ws if len(w) >= 3 and w == w.lower()]
                if is_ascii(value) and len(hits) >= 2:
                    found.append((code, key, value, hits[:4]))
                if code not in ACCENTED_NON_LATIN:
                    continue
            if is_ascii(value):
                allowed = WORD_ALLOW.get(code, set())
                hits = [
                    f"{w} -> {accented[w.lower()].most_common(1)[0][0]}"
                    for w in ws
                    if len(w) >= 3 and w.lower() in accented and w.lower() not in trusted
                    and w.lower() not in allowed
                    # An acronym ("NAS") is not a stripped word ("nås").
                    and not w.isupper()
                ]
                if hits:
                    found.append((code, key, value, hits[:4]))
            if loose:
                hits = []
                for w in ws:
                    lw = w.lower()
                    # All-capitals words are skipped: Greek capitals drop
                    # their accents by rule, and headers are all capitals.
                    if len(lw) < 4 or w.isupper() or strip_accents(lw) != lw or lw not in accented:
                        continue
                    best, n = accented[lw].most_common(1)[0]
                    if n >= 2 * counts[lw]:
                        hits.append(f"{w} -> {best}")
                if hits:
                    candidates.append((code, key, value, hits[:4]))
    return found, candidates


def main():
    args = sys.argv[1:]
    loose = "--loose" in args
    args = [a for a in args if a != "--loose"]
    root = Path(args[0]) if args else Path(__file__).resolve().parents[2]
    found, candidates = check(root, loose)
    for code, key, value, hits in found:
        shown = value if len(value) <= 70 else value[:67] + "..."
        print(f"{code}  {key}: {shown!r}  {hits}")
    print(f"{len(found)} suspect string(s)")
    if loose:
        print("--- loose candidates (review by hand; many are real words) ---")
        for code, key, value, hits in candidates:
            shown = value if len(value) <= 70 else value[:67] + "..."
            print(f"{code}  {key}: {shown!r}  {hits}")
        print(f"{len(candidates)} candidate(s)")
    return 1 if found else 0


if __name__ == "__main__":
    sys.exit(main())
