import { useCallback, useEffect, useMemo, useState } from "react";
import { rpc } from "../ipc";

/** A push-capable git remote, narrowed to the fields the UI cares about. */
export interface PushRemote {
  name: string;
  url: string;
  /** True if this is a `crab://` URL — the only kind that streams pointer data. */
  isCrab: boolean;
}

/** Resolved upstream of the current branch. */
export interface UpstreamInfo {
  /** Full upstream ref, e.g. `origin/main`. Null if no tracking branch is set. */
  full: string | null;
  /** Remote name extracted from `full` (`origin`). Null when no upstream. */
  remote: string | null;
  /** Branch name on the remote (`main`). Null when no upstream. */
  branch: string | null;
  /** Local commits not yet on upstream. */
  ahead: number;
  /** Upstream commits not yet local. */
  behind: number;
}

export interface PushTargetInfo {
  /** Local branch name, or null when HEAD is detached. */
  branch: string | null;
  /** True when HEAD doesn't point at a branch ref (detached). */
  detached: boolean;
  /** Upstream tracking info for the current branch. */
  upstream: UpstreamInfo;
  /** All push-capable remotes on the repo. */
  remotes: PushRemote[];
  /** Subset of `remotes` that speak crab:// — these are the only valid targets. */
  crabRemotes: PushRemote[];
  loading: boolean;
  error: string | null;
  refresh: () => void;
}

/**
 * Resolve everything the Push tab needs to render an honest target
 * card: the current branch, its upstream, ahead/behind counts, and
 * the list of push-capable crab remotes. Wraps three existing RPCs
 * (`list_refs`, `git_branch_info`, `git_repo_info`) so callers don't
 * have to coordinate the three calls themselves.
 *
 * Returns sensible defaults while loading: an empty remotes list and
 * `branch: null`. Surfaces non-fatal errors via `error` rather than
 * throwing, so the UI can degrade to "Push" without ahead/behind.
 */
export function usePushTarget(repoId: string): PushTargetInfo {
  const [data, setData] = useState<{
    branch: string | null;
    detached: boolean;
    upstream: UpstreamInfo;
    remotes: PushRemote[];
  }>(() => emptyTarget());
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const refresh = useCallback(() => {
    setLoading(true);
    setError(null);

    // Two parallel calls: `list_refs` to find HEAD and detect detached
    // state, plus `git_repo_info` for the remote list. The branch-info
    // call is sequenced after we know the branch name.
    Promise.all([
      rpc.listRefs(repoId, { kinds: ["branch"] }).catch(() => ({ refs: [] })),
      rpc.gitRepoInfo(repoId).catch(
        () => ({ remotes: [] as Array<{ name: string; url: string; type: string }> }),
      ),
    ])
      .then(async ([refs, info]) => {
        // The agent emits one row per branch ref with `is_head: true`
        // marking the current branch. There's no synthetic `head`
        // kind ref — looking for one always misses, which produced
        // a phantom "detached HEAD" state.
        const headRef =
          refs.refs.find((r) => r.kind === "branch" && r.is_head === true) ?? null;
        const branch = headRef
          ? headRef.name.replace(/^refs\/heads\//, "")
          : null;
        const detached = !branch;

        // `git remote -v` emits two lines per remote (fetch + push)
        // and the agent dedupes by (name, url) — so by the time we
        // see it, each remote appears once but the surviving `type`
        // is whichever line came first (usually `fetch`). Filtering
        // by type drops perfectly valid remotes. Dedupe by name only
        // and trust that fetch/push URLs match for crab remotes.
        const remotes = uniqueByName(
          info.remotes.map((r) => ({
            name: r.name,
            url: r.url,
            isCrab: r.url.startsWith("crab://"),
          })),
        );

        let upstream = emptyUpstream();
        if (branch) {
          try {
            const bi = await rpc.gitBranchInfo(repoId, branch);
            upstream = parseUpstream(bi.upstream, bi.upstream_ahead, bi.upstream_behind);
          } catch {
            // Non-fatal — branch may have no upstream, or the agent
            // is offline. Empty upstream is the right default.
          }
        }

        setData({ branch, detached, upstream, remotes });
      })
      .catch((e: Error) => setError(e.message))
      .finally(() => setLoading(false));
  }, [repoId]);

  useEffect(() => {
    refresh();
  }, [refresh]);

  const crabRemotes = useMemo(
    () => data.remotes.filter((r) => r.isCrab),
    [data.remotes],
  );

  return {
    ...data,
    crabRemotes,
    loading,
    error,
    refresh,
  };
}

// ── Helpers ─────────────────────────────────────────────────────────

function emptyUpstream(): UpstreamInfo {
  return { full: null, remote: null, branch: null, ahead: 0, behind: 0 };
}

function emptyTarget(): {
  branch: string | null;
  detached: boolean;
  upstream: UpstreamInfo;
  remotes: PushRemote[];
} {
  return {
    branch: null,
    detached: false,
    upstream: emptyUpstream(),
    remotes: [],
  };
}

/**
 * Split an upstream ref like `origin/feature/x` into remote + branch.
 * The remote is always the first slash-delimited segment; the branch
 * is everything after, so multi-segment refs like `feature/x` survive.
 */
export function parseUpstream(
  full: string | null,
  ahead: number,
  behind: number,
): UpstreamInfo {
  if (!full) return emptyUpstream();
  const slash = full.indexOf("/");
  if (slash === -1) {
    // Defensive: a remote-tracking ref without a slash is malformed.
    // Treat the whole string as the remote, with no branch.
    return { full, remote: full, branch: null, ahead, behind };
  }
  return {
    full,
    remote: full.slice(0, slash),
    branch: full.slice(slash + 1),
    ahead,
    behind,
  };
}

function uniqueByName(remotes: PushRemote[]): PushRemote[] {
  const seen = new Map<string, PushRemote>();
  for (const r of remotes) {
    if (!seen.has(r.name)) seen.set(r.name, r);
  }
  return Array.from(seen.values());
}

// ── Persistence ─────────────────────────────────────────────────────

const LS_KEY_PREFIX = "crab.pushRemote.v1.";

/** Read the user's last-selected push remote for `repoId`, if any. */
export function loadPersistedRemote(repoId: string): string | null {
  try {
    return localStorage.getItem(LS_KEY_PREFIX + repoId);
  } catch {
    return null;
  }
}

/** Persist `remote` as the user's chosen target for `repoId`. */
export function persistRemote(repoId: string, remote: string | null): void {
  try {
    if (remote) localStorage.setItem(LS_KEY_PREFIX + repoId, remote);
    else localStorage.removeItem(LS_KEY_PREFIX + repoId);
  } catch {
    // Storage quota/disabled — non-fatal.
  }
}
