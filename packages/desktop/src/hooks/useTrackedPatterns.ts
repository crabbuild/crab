import { useCallback, useEffect, useState } from "react";
import { mainProcess, rpc } from "../ipc";
import { OpenRepo } from "../state";
import { repositoryWorktree } from "../lib/repository-worktree";

/** A pattern parsed from `.gitattributes` that has `filter=crab`. */
export interface TrackedPattern {
  /** The glob as written in `.gitattributes` (e.g. `*.bin`). */
  pattern: string;
}

/** A named prefetch profile from `.crab/prefetch.toml`. */
export interface PrefetchProfile {
  name: string;
  patterns: string[];
}

export interface UseTrackedPatterns {
  patterns: TrackedPattern[];
  profiles: PrefetchProfile[];
  loading: boolean;
  error: string | null;
  refresh: () => void;
}

/**
 * Read the repo's `.gitattributes` patterns marked with `filter=crab`
 * and any prefetch profiles defined in `.crab/prefetch.toml`. Local
 * worktrees use their filesystem provider; SDK snapshots use captured native
 * reads. Failed or truncated reads remain visible instead of becoming defaults.
 *
 * Missing physical files yield an empty result. Snapshot failures remain
 * visible until their native source establishes absence. The hook exposes a
 * manual `refresh()` for callers that just edited tracking state.
 */
export function useTrackedPatterns(repo: OpenRepo, enabled = true): UseTrackedPatterns {
  const [patterns, setPatterns] = useState<TrackedPattern[]>([]);
  const [profiles, setProfiles] = useState<PrefetchProfile[]>([]);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const refresh = useCallback(() => {
    if (!enabled) return;
    setLoading(true);
    setError(null);
    Promise.all([
      readRepoFile(repo, ".gitattributes"),
      readRepoFile(repo, ".crab/prefetch.toml"),
    ])
      .then(([attrs, prefetch]) => {
        setPatterns(parseGitattributes(attrs ?? ""));
        setProfiles(parsePrefetchToml(prefetch ?? ""));
      })
      .catch((e: Error) => setError(e.message))
      .finally(() => setLoading(false));
    // The reducer occasionally hands us a fresh `repo` object with
    // identical fields. Depending on the *stable* identity bits keeps
    // us from re-fetching `.gitattributes` every time another piece
    // of repo state ticks.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [repo.repo_id, repo.path, repo.is_remote, enabled]);

  useEffect(() => {
    refresh();
  }, [refresh]);

  return { patterns, profiles, loading, error, refresh };
}

// ── File reading ────────────────────────────────────────────────────

async function readRepoFile(repo: OpenRepo, relPath: string): Promise<string | null> {
  const root = repositoryWorktree(repo);
  if (root) {
    const res = await mainProcess.fsReadFile(repo.repo_id, `${root.replace(/[\\/]$/, "")}/${relPath}`, 256 * 1024);
    return res.exists ? res.content : null;
  }
  const result = await rpc.readText(repo.repo_id, relPath, { max_bytes: 256 * 1024 });
  if (result.truncated || result.text === null) throw new Error(result.reason ?? "The tracking configuration was not read completely.");
  return result.text;
}

// ── Parsing ─────────────────────────────────────────────────────────

/**
 * Extract the glob from each `.gitattributes` line that contains
 * `filter=crab`. Mirrors the parser in `electron/orchestrator-lfs.ts`
 * but additionally handles patterns wrapped in double quotes (used
 * when the path contains spaces).
 */
export function parseGitattributes(content: string): TrackedPattern[] {
  const out: TrackedPattern[] = [];
  const seen = new Set<string>();
  for (const raw of content.split("\n")) {
    const line = raw.trim();
    if (!line || line.startsWith("#")) continue;
    if (!line.includes("filter=crab")) continue;
    const pattern = extractFirstToken(line);
    if (pattern && !seen.has(pattern)) {
      seen.add(pattern);
      out.push({ pattern });
    }
  }
  return out;
}

/**
 * Pull the first whitespace-delimited token off a `.gitattributes`
 * line, treating a leading `"…"` as a single token (so patterns with
 * spaces survive).
 */
function extractFirstToken(line: string): string | null {
  if (line.startsWith('"')) {
    const end = line.indexOf('"', 1);
    if (end === -1) return null;
    return line.slice(1, end);
  }
  const m = line.match(/^\S+/);
  return m ? m[0] : null;
}

/**
 * Tiny TOML reader for `.crab/prefetch.toml`. Avoids pulling in a
 * full TOML parser — we only care about the `[[profile]]` array of
 * `{ name, paths }` entries, mirroring the rust loader in
 * `crab/src/hydrate/profile.rs`.
 *
 * The format is intentionally narrow:
 *
 *   version = 1
 *   [[profile]]
 *   name = "ci"
 *   paths = ["tests/fixtures/**", "*.bin"]
 *
 * Anything fancier (inline tables, multi-line strings) is ignored.
 * Bad input yields an empty list rather than an error — the desktop
 * UI degrades to "no profiles" gracefully.
 */
export function parsePrefetchToml(content: string): PrefetchProfile[] {
  if (!content.trim()) return [];
  const profiles: PrefetchProfile[] = [];
  let current: { name?: string; paths?: string[] } | null = null;

  const flush = () => {
    if (current && current.name && current.paths) {
      profiles.push({ name: current.name, patterns: current.paths });
    }
    current = null;
  };

  const lines = content.split("\n");
  for (let i = 0; i < lines.length; i++) {
    const line = lines[i].trim();
    if (!line || line.startsWith("#")) continue;

    if (line === "[[profile]]") {
      flush();
      current = {};
      continue;
    }
    if (line.startsWith("[")) {
      // Some other section — ignore it for the active profile.
      flush();
      continue;
    }
    if (!current) continue;

    const eq = line.indexOf("=");
    if (eq === -1) continue;
    const key = line.slice(0, eq).trim();
    const value = line.slice(eq + 1).trim();

    if (key === "name") {
      const m = value.match(/^"([^"]*)"$/) ?? value.match(/^'([^']*)'$/);
      if (m) current.name = m[1];
    } else if (key === "paths") {
      // Single-line array form: `paths = ["a", "b"]`.
      let arr = value;
      if (!arr.endsWith("]")) {
        // Multi-line array — accumulate until the closing bracket.
        const buf: string[] = [arr];
        while (++i < lines.length) {
          buf.push(lines[i]);
          if (lines[i].trim().endsWith("]")) break;
        }
        arr = buf.join(" ");
      }
      current.paths = parseTomlStringArray(arr);
    }
  }
  flush();

  // Stable alphabetical order, matching the rust loader.
  profiles.sort((a, b) => a.name.localeCompare(b.name));
  return profiles;
}

function parseTomlStringArray(input: string): string[] {
  const start = input.indexOf("[");
  const end = input.lastIndexOf("]");
  if (start === -1 || end === -1 || end < start) return [];
  const inner = input.slice(start + 1, end);
  const out: string[] = [];
  // Quoted strings only — single or double, no escaping support
  // beyond what TOML basic strings allow. Sufficient for prefetch
  // patterns which never contain quote characters in practice.
  const re = /"((?:[^"\\]|\\.)*)"|'([^']*)'/g;
  let m: RegExpExecArray | null;
  while ((m = re.exec(inner)) !== null) {
    const v = m[1] ?? m[2] ?? "";
    out.push(v.replace(/\\"/g, '"').replace(/\\\\/g, "\\"));
  }
  return out;
}
