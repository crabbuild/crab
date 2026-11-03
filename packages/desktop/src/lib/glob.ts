/**
 * Minimal glob matcher used by the Hydrate scope picker.
 *
 * Semantics:
 *   `*`   — matches anything but `/`
 *   `**`  — matches across path segments (including empty)
 *   `?`   — matches a single non-`/` character
 *   `[..]` — passed through to the regex engine (character class)
 *
 * Good enough for `.gitattributes` patterns like `*.bin`, `data/**`,
 * and `models/*.safetensors`. Not a full POSIX glob; we don't need
 * brace expansion or extglob here.
 */
export function compileGlob(pattern: string): (path: string) => boolean {
  const re = globToRegExp(pattern);
  return (path) => re.test(path);
}

/** Compile to a `RegExp` directly when callers want to memoize. */
export function globToRegExp(pattern: string): RegExp {
  // Escape regex metacharacters that aren't part of glob syntax,
  // then translate `**`, `*`, and `?` in a stable order. We mark
  // `**` first via a placeholder so it isn't mangled by the `*`
  // pass.
  const esc = pattern
    .replace(/[.+^$()|{}\\]/g, "\\$&")
    .replace(/\*\*/g, "__GLOB_DOUBLE__")
    .replace(/\*/g, "[^/]*")
    .replace(/__GLOB_DOUBLE__/g, ".*")
    .replace(/\?/g, "[^/]");
  return new RegExp("^" + esc + "$");
}

/**
 * Returns true if `path` is matched by any of `patterns`. Empty
 * pattern list returns `false` (matches nothing) — callers that want
 * "empty = match all" should special-case it.
 */
export function matchesAny(path: string, patterns: readonly string[]): boolean {
  for (const p of patterns) {
    if (compileGlob(p)(path)) return true;
  }
  return false;
}
