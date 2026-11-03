/**
 * Delta updates for git_status_porcelain responses.
 *
 * Tracks the last full status response per repo and computes a hash
 * for comparison. When the remote agent supports delta mode, it can
 * return only changed entries instead of the full status, reducing
 * payload size for large repos.
 *
 * Protocol:
 * - Client sends `last_hash` with each `git_status_porcelain` request.
 * - If the remote agent recognizes the hash, it returns a delta
 *   (added/removed/modified entries) instead of the full response.
 * - If the hash doesn't match (or is null), the agent returns the
 *   full response as usual.
 * - The client applies the delta to reconstruct the full status.
 */

import { createHash } from "node:crypto";
import type { RpcRouter } from "./ssh-router";

// ── Types ────────────────────────────────────────────────────────

/** A single entry from git status porcelain output. */
export interface StatusEntry {
  /** Two-character status code (e.g., "M ", " M", "??", "A "). */
  status: string;
  /** File path relative to repo root. */
  path: string;
  /** Original path for renames (R status). */
  origPath?: string;
}

/** A delta entry describing a change from the last known state. */
export interface DeltaEntry {
  /** The type of change: add a new entry, remove an existing one, or modify. */
  action: "add" | "remove" | "modify";
  /** File path this delta applies to. */
  path: string;
  /** The new/modified entry data (present for "add" and "modify"). */
  entry?: StatusEntry;
}

/** Response from the remote agent when delta mode is active. */
export interface DeltaResponse {
  /** Hash of the full status after applying this delta. */
  hash: string;
  /** Whether this is a delta response (true) or full response (false). */
  is_delta: boolean;
  /** Full entries (when is_delta=false) or delta entries (when is_delta=true). */
  entries: StatusEntry[] | DeltaEntry[];
}

/** Full status response (non-delta, standard format). */
export interface FullStatusResponse {
  entries: StatusEntry[];
}

/** Internal state tracked per repo. */
interface TrackedState {
  /** SHA-256 hash of the last known full status (sorted, deterministic). */
  hash: string;
  /** The last known full set of status entries. */
  entries: StatusEntry[];
  /** Timestamp of when this state was last updated. */
  updatedAt: number;
}

// ── StatusDeltaTracker ───────────────────────────────────────────

export class StatusDeltaTracker {
  /** Tracked state per repo, keyed by repo_id. */
  private states = new Map<string, TrackedState>();
  /** Maximum number of repos to track (prevents unbounded memory growth). */
  private readonly maxTrackedRepos: number;

  constructor(options?: { maxTrackedRepos?: number }) {
    this.maxTrackedRepos = options?.maxTrackedRepos ?? 100;
  }

  /**
   * Get the hash of the last known status for a repo. Returns null
   * if no previous status is tracked (first request or evicted).
   *
   * Include this value as `last_hash` in `git_status_porcelain` params
   * so the remote agent can decide whether to send a delta.
   */
  getLastHash(repoId: string): string | null {
    const state = this.states.get(repoId);
    return state?.hash ?? null;
  }

  /**
   * Get the last known full status entries for a repo. Returns null
   * if no previous status is tracked.
   */
  getLastEntries(repoId: string): StatusEntry[] | null {
    const state = this.states.get(repoId);
    return state?.entries ?? null;
  }

  /**
   * Process a response from the remote agent. Handles both full
   * responses and delta responses.
   *
   * - If `response.is_delta` is true, applies the delta to the last
   *   known state and returns the reconstructed full status.
   * - If `response.is_delta` is false (or the response is a plain
   *   full response), stores it as the new baseline.
   *
   * Returns the full set of status entries after processing.
   */
  processResponse(repoId: string, response: DeltaResponse | FullStatusResponse): StatusEntry[] {
    if (isDeltaResponse(response)) {
      if (response.is_delta) {
        return this.applyDelta(repoId, response);
      }
      // Full response with hash — store as new baseline
      const entries = response.entries as StatusEntry[];
      this.storeState(repoId, entries, response.hash);
      return entries;
    }

    // Plain full response (no hash field) — compute hash and store
    const entries = response.entries;
    const hash = computeStatusHash(entries);
    this.storeState(repoId, entries, hash);
    return entries;
  }

  /**
   * Clear tracked state for a specific repo. Call this when a repo
   * is closed or the connection is lost.
   */
  clear(repoId: string): void {
    this.states.delete(repoId);
  }

  /**
   * Clear all tracked state. Call this on disconnect or when the
   * cache is fully invalidated.
   */
  clearAll(): void {
    this.states.clear();
  }

  /**
   * Get the number of repos currently being tracked.
   */
  get size(): number {
    return this.states.size;
  }

  /**
   * Compute the hash for a set of status entries. Exposed for testing
   * and for the remote agent to use the same algorithm.
   */
  static computeHash(entries: StatusEntry[]): string {
    return computeStatusHash(entries);
  }

  // ── Private ────────────────────────────────────────────────────

  /**
   * Apply a delta response to the last known state for a repo.
   * Returns the reconstructed full status entries.
   *
   * If no previous state exists (shouldn't happen if the remote agent
   * is behaving correctly), falls back to treating delta entries as
   * the full state (best effort).
   */
  private applyDelta(repoId: string, response: DeltaResponse): StatusEntry[] {
    const lastState = this.states.get(repoId);
    if (!lastState) {
      // No baseline — can't apply delta. This shouldn't happen in
      // normal operation (the agent only sends deltas when it has a
      // matching hash). Treat as error and return empty.
      return [];
    }

    const deltaEntries = response.entries as DeltaEntry[];
    const result = applyDeltaToEntries(lastState.entries, deltaEntries);

    this.storeState(repoId, result, response.hash);
    return result;
  }

  /**
   * Store a new baseline state for a repo. Evicts the oldest tracked
   * repo if at capacity.
   */
  private storeState(repoId: string, entries: StatusEntry[], hash: string): void {
    // Evict oldest if at capacity and this is a new repo
    if (!this.states.has(repoId) && this.states.size >= this.maxTrackedRepos) {
      this.evictOldest();
    }

    this.states.set(repoId, {
      hash,
      entries: [...entries],
      updatedAt: Date.now(),
    });
  }

  /**
   * Evict the least recently updated tracked repo.
   */
  private evictOldest(): void {
    let oldestKey: string | null = null;
    let oldestTime = Infinity;

    for (const [key, state] of this.states) {
      if (state.updatedAt < oldestTime) {
        oldestTime = state.updatedAt;
        oldestKey = key;
      }
    }

    if (oldestKey) {
      this.states.delete(oldestKey);
    }
  }
}

// ── Helpers ──────────────────────────────────────────────────────

/**
 * Compute a deterministic SHA-256 hash of a set of status entries.
 * Entries are sorted by path to ensure the hash is stable regardless
 * of the order the remote agent returns them.
 */
export function computeStatusHash(entries: StatusEntry[]): string {
  // Sort by path for deterministic ordering
  const sorted = [...entries].sort((a, b) => a.path.localeCompare(b.path));

  // Build a canonical string representation
  const canonical = sorted
    .map((e) => {
      const base = `${e.status}\t${e.path}`;
      return e.origPath ? `${base}\t${e.origPath}` : base;
    })
    .join("\n");

  return createHash("sha256").update(canonical, "utf8").digest("hex");
}

/**
 * Apply delta entries to a base set of status entries.
 * Returns the new full set of entries after applying all deltas.
 */
export function applyDeltaToEntries(
  base: StatusEntry[],
  deltas: DeltaEntry[],
): StatusEntry[] {
  // Index base entries by path for efficient lookup
  const entryMap = new Map<string, StatusEntry>();
  for (const entry of base) {
    entryMap.set(entry.path, entry);
  }

  for (const delta of deltas) {
    switch (delta.action) {
      case "add":
        if (delta.entry) {
          entryMap.set(delta.path, delta.entry);
        }
        break;
      case "remove":
        entryMap.delete(delta.path);
        break;
      case "modify":
        if (delta.entry) {
          entryMap.set(delta.path, delta.entry);
        }
        break;
    }
  }

  return [...entryMap.values()];
}

/**
 * Type guard to distinguish a DeltaResponse from a plain FullStatusResponse.
 */
function isDeltaResponse(
  response: DeltaResponse | FullStatusResponse,
): response is DeltaResponse {
  return "hash" in response && "is_delta" in response;
}

// ── Router Integration ───────────────────────────────────────────

/**
 * Wraps a `git_status_porcelain` RPC call to include delta tracking.
 * Adds `last_hash` to the request params and processes the response
 * through the delta tracker.
 *
 * Usage in the RPC routing layer:
 * ```ts
 * const tracker = new StatusDeltaTracker();
 *
 * // When routing git_status_porcelain:
 * const entries = await requestWithDelta(router, tracker, repoId, params);
 * ```
 */
export async function requestWithDelta(
  router: RpcRouter,
  tracker: StatusDeltaTracker,
  repoId: string,
  params: Record<string, unknown>,
): Promise<StatusEntry[]> {
  const lastHash = tracker.getLastHash(repoId);

  // Include last_hash in the request so the remote agent can decide
  // whether to send a delta or full response
  const enrichedParams = {
    ...params,
    last_hash: lastHash,
  };

  try {
    const response = await router.route(
      repoId,
      "git_status_porcelain",
      enrichedParams,
    ) as DeltaResponse | FullStatusResponse;

    return tracker.processResponse(repoId, response);
  } catch (err) {
    // On error, fall back to a regular request without delta params.
    // This handles the case where the remote agent doesn't support
    // the last_hash parameter yet.
    const response = await router.route(
      repoId,
      "git_status_porcelain",
      params,
    ) as FullStatusResponse;

    return tracker.processResponse(repoId, response);
  }
}
