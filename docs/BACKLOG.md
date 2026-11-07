# Backlog

Known issues that are not fixed yet.  Each says what is wrong, where, and
what a fix looks like.

## The language list's "Auto" option is always in English

`Settings > Language` offers an "Auto" option (follow the system language)
whose label is hard-coded in English, so it reads "Auto" in every language
while the rest of the screen is translated.  The translated help text
(`gui.recover.settings.language_desc`) names the option as "Auto" in each
language so the two at least agree.

Fix: give the option a translated label from the locale files, as Nyx
Backup did in 1.0.7 (`gui.settings.language_auto`, "System default"), and
then reword `language_desc` in each locale to name the translated option.
Found in the November 2026 translation review.
