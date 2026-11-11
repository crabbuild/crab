/**
 * Hydrate scope resolution.
 *
 * The Hydrate tab lets users pick *what* to hydrate or dehydrate via
 * a structured scope picker rather than a freeform glob textarea.
 * Each [`Scope`] mode is resolved against the repo's hydration state
 * to produce the exact file selection.
 *
 * The verb (hydrate vs dehydrate) is applied as a final filter:
 * hydrate skips files already hydrated, dehydrate skips files that
 * are already pointer-only. This is what users almost always want
 * and matches the "0 will be skipped" preview in the UI.
 */

import { HydrationEntry } from "../ipc";
import { matchesAny } from "./glob";

/** Verb governs which files are *eligible* for an action. */
export type HydrateVerb = "hydrate" | "dehydrate";

/** Discriminated union of scope modes. */
export type Scope =
  | { kind: "all" }
  | { kind: "patterns"; patterns: string[] }
  | { kind: "extensions"; extensions: string[] }
  | { kind: "folders"; folders: string[] }
  | { kind: "custom"; paths: string[] }
  | { kind: "profile"; name: string; patterns: string[] };

export interface ResolvedScope {
  /** Tracked entries that match the scope and are eligible for the verb. */
  matched: HydrationEntry[];
  /** Tracked entries that match the scope but are ineligible for this verb. */
  skipped: HydrationEntry[];
  /** Total logical bytes across `matched` (what the action will affect). */
  bytes: number;
}

/** True if a file is eligible for the verb (i.e. not already in target state). */
export function isEligible(entry: HydrationEntry, verb: HydrateVerb): boolean {
  if (verb === "hydrate") {
    return entry.state === "pointer" || entry.state === "missing";
  }
  return entry.state === "hydrated";
}

/**
 * Resolve a scope against the current hydration entries and verb.
 * `entries` is the full tracked-file list from `useHydrationStatus`.
 */
export function resolveScope(
  scope: Scope,
  entries: readonly HydrationEntry[],
  verb: HydrateVerb,
): ResolvedScope {
  // Step 1: apply the scope filter to get candidate entries.
  const candidates = filterByScope(scope, entries);

  // Split into eligible and skipped entries.
  const matched: HydrationEntry[] = [];
  const skipped: HydrationEntry[] = [];
  for (const e of candidates) {
    if (isEligible(e, verb)) matched.push(e);
    else skipped.push(e);
  }

  const bytes = matched.reduce((sum, e) => sum + e.logical, 0);

  return { matched, skipped, bytes };
}

function filterByScope(scope: Scope, entries: readonly HydrationEntry[]): HydrationEntry[] {
  switch (scope.kind) {
    case "all":
      return entries.slice();

    case "patterns": {
      if (scope.patterns.length === 0) return [];
      return entries.filter((e) => matchesAny(e.path, scope.patterns));
    }

    case "extensions": {
      if (scope.extensions.length === 0) return [];
      const set = new Set(scope.extensions.map((e) => e.toLowerCase()));
      return entries.filter((e) => set.has(extOf(e.path)));
    }

    case "folders": {
      if (scope.folders.length === 0) return [];
      return entries.filter((e) => scope.folders.some((f) => isUnder(e.path, f)));
    }

    case "custom": {
      if (scope.paths.length === 0) return [];
      const set = new Set(scope.paths);
      return entries.filter((e) => set.has(e.path));
    }

    case "profile": {
      if (scope.patterns.length === 0) return [];
      return entries.filter((e) => matchesAny(e.path, scope.patterns));
    }
  }
}

/**
 * Lowercase extension including the leading dot (e.g. `.bin`). Files
 * without an extension return an empty string. Hidden files like
 * `.gitattributes` count as extensionless rather than the whole name
 * being treated as the extension.
 */
export function extOf(path: string): string {
  const slash = path.lastIndexOf("/");
  const base = slash === -1 ? path : path.slice(slash + 1);
  if (base.startsWith(".") && base.indexOf(".", 1) === -1) return "";
  const dot = base.lastIndexOf(".");
  if (dot <= 0) return "";
  return base.slice(dot).toLowerCase();
}

/** True if `path` is `folder` itself or descends from it. */
export function isUnder(path: string, folder: string): boolean {
  if (folder === "" || folder === "/") return true;
  const f = folder.endsWith("/") ? folder.slice(0, -1) : folder;
  return path === f || path.startsWith(f + "/");
}

// ── Aggregations for the picker UI ──────────────────────────────────

export interface ExtensionBucket {
  ext: string; // includes leading dot, lowercased; "" for extensionless
  count: number;
  bytes: number;
}

/** Group entries by extension, sorted by total bytes descending. */
export function bucketByExtension(entries: readonly HydrationEntry[]): ExtensionBucket[] {
  const map = new Map<string, ExtensionBucket>();
  for (const e of entries) {
    const ext = extOf(e.path);
    let b = map.get(ext);
    if (!b) {
      b = { ext, count: 0, bytes: 0 };
      map.set(ext, b);
    }
    b.count += 1;
    b.bytes += e.logical;
  }
  return Array.from(map.values()).sort((a, b) => b.bytes - a.bytes);
}

export interface FolderBucket {
  folder: string;
  count: number;
  bytes: number;
}

/**
 * Group entries by their immediate top-level folder. Files at the
 * repo root land in the synthetic `(root)` bucket. Sorted by bytes
 * descending so the heavy hitters are obvious.
 */
export function bucketByTopFolder(entries: readonly HydrationEntry[]): FolderBucket[] {
  const map = new Map<string, FolderBucket>();
  for (const e of entries) {
    const slash = e.path.indexOf("/");
    const folder = slash === -1 ? "" : e.path.slice(0, slash);
    let b = map.get(folder);
    if (!b) {
      b = { folder, count: 0, bytes: 0 };
      map.set(folder, b);
    }
    b.count += 1;
    b.bytes += e.logical;
  }
  return Array.from(map.values()).sort((a, b) => b.bytes - a.bytes);
}

export interface PatternBucket {
  pattern: string;
  count: number;
  bytes: number;
}

/** For each tracked pattern, count how many entries it matches. */
export function bucketByPattern(
  patterns: readonly string[],
  entries: readonly HydrationEntry[],
): PatternBucket[] {
  return patterns.map((pattern) => {
    let count = 0;
    let bytes = 0;
    for (const e of entries) {
      if (matchesAny(e.path, [pattern])) {
        count += 1;
        bytes += e.logical;
      }
    }
    return { pattern, count, bytes };
  });
}
