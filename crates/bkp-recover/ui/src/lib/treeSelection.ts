/**
 * Shared selection / exclusion rules for the two path trees.
 *
 * Both the restore tree (`SnapshotFileTree.svelte`) and the backup-set source
 * picker (`FilesystemTree.svelte`) describe a chosen subtree as two sets of
 * path PREFIXES: selections and exclusions.  The most specific rule covering a
 * path decides what happens to it.
 *
 * This module is the single implementation of that idea on the TypeScript
 * side.  It is a deliberate mirror of `bkp_restore::path_selected` /
 * `dir_selected`, and `treeSelection.test.ts` holds a case table that must
 * match the Rust tests case for case.  Nothing in the build checks the two
 * against each other, so a change here needs the same change there: the
 * failure mode is not a compile error but a tree that paints a tick on a file
 * the restore will skip, which is how every bug in this area has presented.
 *
 * Deliberately free of runes and of any Svelte import, so it is testable as
 * plain TypeScript.
 */

/** Tri-state of a row: fully selected, partially selected, not selected. */
export type CheckState = 'full' | 'partial' | 'none'

/** The winning rule for a path: whether it selects or excludes. */
export interface Rule {
  include: boolean
}

/**
 * True when `child` lies strictly beneath `parent`.
 *
 * Compares whole path segments, so `a/bc` is NOT inside `a/b`.  A plain
 * `startsWith` would say it is, and would quietly widen every exclusion to its
 * name-prefix siblings.  Both separators are accepted because snapshot paths
 * arrive with whichever the source machine used.
 */
export function isPathInside(child: string, parent: string): boolean {
  if (child === parent) return false
  const parentEndsInSep = parent.endsWith('/') || parent.endsWith('\\')
  if (parentEndsInSep) return child.startsWith(parent)
  return child.startsWith(parent + '/') || child.startsWith(parent + '\\')
}

/** True when `path` is `rule` itself or lies beneath it. */
export function isAtOrUnder(path: string, rule: string): boolean {
  return path === rule || isPathInside(path, rule)
}

/**
 * Resolve the most specific rule covering `path`, or `null` when none does.
 *
 * Specificity is the rule's string length, which works because a longer rule
 * that still covers `path` is necessarily deeper.  On a tie - the same path in
 * both sets - the exclusion wins: not restoring a file is the recoverable
 * mistake.
 */
export function ruleFor(
  path: string,
  selection: Iterable<string>,
  excluded: Iterable<string>,
): Rule | null {
  let bestLen = -1
  let include = false
  for (const s of selection) {
    if (isAtOrUnder(path, s) && s.length > bestLen) {
      bestLen = s.length
      include = true
    }
  }
  for (const e of excluded) {
    if (isAtOrUnder(path, e) && e.length >= bestLen) {
      bestLen = e.length
      include = false
    }
  }
  return bestLen < 0 ? null : { include }
}

/** True when the nearest rule covering `path` is an exclusion. */
export function isRuledOut(
  path: string,
  selection: Iterable<string>,
  excluded: Iterable<string>,
): boolean {
  return ruleFor(path, selection, excluded)?.include === false
}

/** Whether any member of `paths` lies strictly beneath `path`. */
export function hasDescendantIn(path: string, paths: Iterable<string>): boolean {
  for (const p of paths) {
    if (isPathInside(p, path)) return true
  }
  return false
}

/**
 * Tri-state for one row.
 *
 * `descendantsOf` is consulted only for a directory with no rule of its own
 * and is expected to be expensive, so it is a callback rather than a value.
 */
export function stateFor(
  path: string,
  isDir: boolean,
  selection: Iterable<string>,
  excluded: Iterable<string>,
  descendantsOf?: (dir: string) => string[],
): CheckState {
  const rule = ruleFor(path, selection, excluded)

  if (rule) {
    if (!isDir) return rule.include ? 'full' : 'none'
    // A directory's own rule is only the baseline: any rule strictly beneath
    // it overrides part of the subtree, which is what 'partial' means.
    if (rule.include) {
      return hasDescendantIn(path, excluded) ? 'partial' : 'full'
    }
    return hasDescendantIn(path, selection) ? 'partial' : 'none'
  }

  if (!isDir || !descendantsOf) return 'none'
  const paths = descendantsOf(path)
  if (paths.length === 0) return 'none'
  const included = paths.filter(p => ruleFor(p, selection, excluded)?.include === true).length
  if (included === 0) return 'none'
  return included === paths.length ? 'full' : 'partial'
}

/**
 * Flip one path's effective state, writing the fewest rules that express it.
 *
 * Every rule at or beneath `path` is dropped first.  That is load-bearing, not
 * tidiness: a rule beneath the toggled path is unreachable under the NEW rule
 * but comes back under a later one.  Exclude a folder, select one file inside
 * it, then toggle the folder off again, and that stale selection is still the
 * more specific rule - so the folder reads "none of me" while one child stays
 * selected.
 *
 * An explicit rule is then added only when the wanted state differs from what
 * the path inherits from its remaining ancestors, so toggling a child back to
 * match its parent leaves no residue and the two sets do not grow one entry
 * per click.
 *
 * Returns NEW sets; the inputs are not mutated.
 *
 * Invariant relied upon by the callers: this always changes the path's
 * effective state, because the new state is `!wasIncluded` by construction.
 * That is what lets the checkbox attachment re-run on every click and assert
 * the painted value over whatever the browser did to the element.
 */
export function toggleRule(
  path: string,
  selection: Set<string>,
  excluded: Set<string>,
): { selection: Set<string>; excluded: Set<string> } {
  const wasIncluded = ruleFor(path, selection, excluded)?.include === true

  const sel = new Set(selection)
  const exc = new Set(excluded)
  for (const p of [...sel]) {
    if (isAtOrUnder(p, path)) sel.delete(p)
  }
  for (const p of [...exc]) {
    if (isAtOrUnder(p, path)) exc.delete(p)
  }

  const inherited = ruleFor(path, sel, exc)?.include === true
  const want = !wasIncluded

  if (want !== inherited) {
    if (want) sel.add(path)
    else exc.add(path)
  }

  return { selection: sel, excluded: exc }
}

/**
 * Drop every rule in `set` at or beneath `path`.
 *
 * Used when a source path is removed outright (rather than toggled), so its
 * exclusions do not linger as orphans that silently reapply if the same folder
 * is added again later.
 *
 * Returns a NEW set.
 */
export function withoutDescendants(set: Set<string>, path: string): Set<string> {
  const next = new Set(set)
  for (const p of [...next]) {
    if (isAtOrUnder(p, path)) next.delete(p)
  }
  return next
}
