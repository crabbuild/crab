// Classify repository files into ML/data categories.
//
// The classifier is a pure function: given a file's path, size, and
// pointer kind, it returns one of the eight dataset kinds. Rules are
// declarative (globs + size thresholds) loaded from a JSON file so
// users can override per-workspace without touching code.
//
// Glob matching is a minimal subset (** for recursive directory
// match, * for single-segment wildcard, ? for single character)
// since the patterns in dataset-rules.json are well-known and
// limited. No external glob library needed.

import type { PointerKind } from "../ipc";
import defaultRulesJson from "../assets/dataset-rules.json";

export type DatasetKind =
  | "training-data"
  | "eval-data"
  | "checkpoint"
  | "tokenizer"
  | "notebook"
  | "config"
  | "code"
  | "other";

export interface DatasetRule {
  kind: DatasetKind;
  globs: string[];
  /** Minimum file size in bytes. When set, the rule only matches
   *  files at or above this threshold. */
  minBytes?: number;
  /** When true, the rule only matches pointer files (crab or lfs). */
  pointerOnly?: boolean;
}

export interface DatasetRulesFile {
  rules: DatasetRule[];
  /** When true, the workspace ruleset replaces the defaults entirely
   *  rather than prepending to them. */
  replace?: boolean;
}

/** Default rules shipped with the app. */
export const defaultRules: DatasetRulesFile = defaultRulesJson as DatasetRulesFile;

export interface ClassifyInput {
  path: string;
  size: number | null;
  pointerKind: PointerKind | null;
}

/**
 * Classify a file against the given rules. First match wins;
 * unmatched files return `"other"`.
 */
export function classifyFile(input: ClassifyInput, rules: DatasetRule[]): DatasetKind {
  for (const rule of rules) {
    if (rule.pointerOnly && !input.pointerKind) continue;
    if (rule.minBytes != null && (input.size == null || input.size < rule.minBytes)) continue;

    for (const glob of rule.globs) {
      if (globMatch(glob, input.path)) {
        return rule.kind;
      }
    }
  }
  return "other";
}

// --- Minimal glob matcher ---
//
// Supports:
//   **  — matches zero or more path segments (including separators)
//   *   — matches zero or more characters within a single segment
//   ?   — matches exactly one non-separator character
//
// All comparisons are case-insensitive to handle mixed-case
// filenames on case-preserving filesystems.

/**
 * Test whether `path` matches a glob pattern. Both are normalised
 * to forward slashes and lowercased before matching.
 */
export function globMatch(pattern: string, path: string): boolean {
  const p = pattern.toLowerCase().replace(/\\/g, "/");
  const s = path.toLowerCase().replace(/\\/g, "/");
  return matchSegments(p, 0, s, 0);
}

function matchSegments(pattern: string, pi: number, str: string, si: number): boolean {
  while (pi < pattern.length && si < str.length) {
    if (pattern[pi] === "*" && pi + 1 < pattern.length && pattern[pi + 1] === "*") {
      // Consume the ** and any trailing slash
      let nextPi = pi + 2;
      if (nextPi < pattern.length && pattern[nextPi] === "/") nextPi++;

      // ** matches zero or more path segments — try every suffix
      for (let i = si; i <= str.length; i++) {
        if (matchSegments(pattern, nextPi, str, i)) return true;
      }
      return false;
    }

    if (pattern[pi] === "*") {
      // Single * — match within one segment (no slashes)
      const nextPi = pi + 1;
      for (let i = si; i <= str.length; i++) {
        if (i > si && str[i - 1] === "/") break;
        if (matchSegments(pattern, nextPi, str, i)) return true;
      }
      return false;
    }

    if (pattern[pi] === "?") {
      // ? matches one non-separator character
      if (str[si] === "/") return false;
      pi++;
      si++;
      continue;
    }

    // Literal character comparison
    if (pattern[pi] !== str[si]) return false;
    pi++;
    si++;
  }

  // Consume trailing wildcards that can match empty
  while (pi < pattern.length) {
    if (pattern[pi] === "*" && pi + 1 < pattern.length && pattern[pi + 1] === "*") {
      pi += 2;
      if (pi < pattern.length && pattern[pi] === "/") pi++;
    } else if (pattern[pi] === "*") {
      pi++;
    } else {
      break;
    }
  }

  return pi === pattern.length && si === str.length;
}


/**
 * Merge workspace-level dataset rules with the shipped defaults.
 *
 * - If `overrides.replace` is true, the workspace rules completely
 *   replace the defaults.
 * - Otherwise workspace rules are prepended so they take priority
 *   (first match wins in `classifyFile`).
 */
export function mergeRules(defaults: DatasetRule[], overrides: DatasetRulesFile): DatasetRule[] {
  if (overrides.replace) {
    return overrides.rules;
  }
  return [...overrides.rules, ...defaults];
}
