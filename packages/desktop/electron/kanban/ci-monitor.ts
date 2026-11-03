/**
 * CI Monitor for the Kanban subsystem.
 *
 * Polls PR check status and review state for watched PRs, emitting events
 * only on state transitions. Implements adaptive polling (doubles interval
 * on consecutive failures) and auto-unwatches PRs that close or merge.
 */

import { EventEmitter } from "node:events";
import log from "electron-log";

// ── Constants ────────────────────────────────────────────────────

export const MIN_POLL_INTERVAL_MS = 10_000;
export const MAX_POLL_INTERVAL_MS = 300_000;
export const DEFAULT_POLL_INTERVAL_MS = 30_000;

const CONSECUTIVE_FAILURES_THRESHOLD = 3;

// ── Interfaces ───────────────────────────────────────────────────

/** Minimal interface for PR check data (matches orchestrator-pr.ts shape). */
export interface CICheck {
  name: string;
  status: "pass" | "fail" | "pending" | "queued";
  conclusion?: string;
  durationMs?: number;
  url?: string;
  output?: string;
}

/** Minimal interface for PR metadata from the forge/gh CLI. */
export interface PRDetail {
  title: string;
  number: number;
  url: string;
  state: string;
  reviewers: Array<{ login: string; state: string }>;
  mergeable: boolean;
  mergeStateStatus: string;
}

/** Minimal interface for review comments. */
export interface PRComment {
  id: string;
  body: string;
  path?: string;
  line?: number;
  resolved: boolean;
}

/** Combined PR status data returned by the data provider. */
export interface PRChecksData {
  pr?: PRDetail;
  checks: CICheck[];
  comments: PRComment[];
}

/**
 * Interface for the data provider that fetches PR status.
 * In production this wraps getPRChecks from orchestrator-pr.ts and forge RPC.
 */
export interface PRDataProvider {
  getPRChecks(repoPath: string, prNumber: number): Promise<{ success: boolean; data?: PRChecksData; error?: string }>;
}

/** Tracked state for a single watched PR. */
export interface WatchedPR {
  prNumber: number;
  cardId: string;
  lastChecksStatus: "pass" | "fail" | "pending" | "unknown";
  lastReviewState: string | null;
  lastCommentIds: Set<string>;
  lastPRState: string | null;
}

/** Serializable version of WatchedPR for getWatchedPRs(). */
export interface WatchedPRInfo {
  prNumber: number;
  cardId: string;
  lastChecksStatus: string;
  lastReviewState: string | null;
}

// ── CIMonitor ────────────────────────────────────────────────────

export interface CIMonitorConfig {
  pollIntervalMs: number;
  repoPath: string;
}

export class CIMonitor extends EventEmitter {
  private config: CIMonitorConfig;
  private provider: PRDataProvider;
  private watchedPRs: Map<number, WatchedPR> = new Map();
  private pollTimer: ReturnType<typeof setInterval> | null = null;
  private polling = false;
  private consecutiveFailures = 0;
  private currentIntervalMs: number;
  private baseIntervalMs: number;
  private degraded = false;

  constructor(config: CIMonitorConfig, provider: PRDataProvider) {
    super();
    this.config = config;
    this.provider = provider;
    this.baseIntervalMs = clampInterval(config.pollIntervalMs);
    this.currentIntervalMs = this.baseIntervalMs;
  }

  /** Add a PR to the watched set with initial unknown state. */
  watchPR(prNumber: number, cardId: string): void {
    if (this.watchedPRs.has(prNumber)) {
      log.debug(`CIMonitor: PR #${prNumber} already watched`);
      return;
    }

    this.watchedPRs.set(prNumber, {
      prNumber,
      cardId,
      lastChecksStatus: "unknown",
      lastReviewState: null,
      lastCommentIds: new Set(),
      lastPRState: null,
    });

    log.info(`CIMonitor: watching PR #${prNumber} for card ${cardId}`);

    // Start polling if not already running and we have PRs to watch
    if (!this.pollTimer) {
      this.startPolling();
    }
  }

  /** Remove a PR from the watched set. */
  unwatchPR(prNumber: number): void {
    const removed = this.watchedPRs.delete(prNumber);
    if (removed) {
      log.info(`CIMonitor: unwatched PR #${prNumber}`);
    }

    // Stop polling if no PRs remain
    if (this.watchedPRs.size === 0) {
      this.stopPolling();
    }
  }

  /** Get a snapshot of all currently watched PRs. */
  getWatchedPRs(): WatchedPRInfo[] {
    const result: WatchedPRInfo[] = [];
    for (const pr of this.watchedPRs.values()) {
      result.push({
        prNumber: pr.prNumber,
        cardId: pr.cardId,
        lastChecksStatus: pr.lastChecksStatus,
        lastReviewState: pr.lastReviewState,
      });
    }
    return result;
  }

  /** Stop polling and clean up. */
  stop(): void {
    this.stopPolling();
    this.watchedPRs.clear();
  }

  // ── Private ──────────────────────────────────────────────────

  private startPolling(): void {
    if (this.pollTimer) return;

    this.pollTimer = setInterval(() => {
      void this.pollAll();
    }, this.currentIntervalMs);

    // Run an immediate poll
    void this.pollAll();
  }

  private stopPolling(): void {
    if (this.pollTimer) {
      clearInterval(this.pollTimer);
      this.pollTimer = null;
    }
  }

  private restartPollingWithInterval(intervalMs: number): void {
    this.stopPolling();
    this.currentIntervalMs = intervalMs;
    if (this.watchedPRs.size > 0) {
      this.pollTimer = setInterval(() => {
        void this.pollAll();
      }, this.currentIntervalMs);
    }
  }

  private async pollAll(): Promise<void> {
    if (this.polling) return;
    this.polling = true;

    try {
      const prNumbers = [...this.watchedPRs.keys()];
      let anyFailure = false;

      for (const prNumber of prNumbers) {
        const watched = this.watchedPRs.get(prNumber);
        if (!watched) continue;

        try {
          const result = await this.provider.getPRChecks(this.config.repoPath, prNumber);

          if (!result.success || !result.data) {
            anyFailure = true;
            log.warn(`CIMonitor: failed to fetch PR #${prNumber}: ${result.error}`);
            continue;
          }

          this.processTransitions(watched, result.data);
        } catch (err) {
          anyFailure = true;
          const message = err instanceof Error ? err.message : String(err);
          log.warn(`CIMonitor: error polling PR #${prNumber}: ${message}`);
        }
      }

      if (anyFailure) {
        this.handlePollFailure();
      } else {
        this.handlePollSuccess();
      }
    } finally {
      this.polling = false;
    }
  }

  private processTransitions(watched: WatchedPR, data: PRChecksData): void {
    // Check if PR is closed/merged — auto-unwatch
    if (data.pr) {
      const prState = data.pr.state.toUpperCase();
      if (prState === "CLOSED" || prState === "MERGED") {
        this.emit("monitor:pr_closed", {
          prNumber: watched.prNumber,
          cardId: watched.cardId,
          finalState: prState.toLowerCase(),
        });
        this.unwatchPR(watched.prNumber);
        return;
      }
      watched.lastPRState = prState;
    }

    // Detect CI check transitions
    this.detectCheckTransitions(watched, data.checks);

    // Detect review transitions
    this.detectReviewTransitions(watched, data);
  }

  private detectCheckTransitions(watched: WatchedPR, checks: CICheck[]): void {
    if (checks.length === 0) return;

    const currentStatus = deriveOverallCheckStatus(checks);

    if (currentStatus === watched.lastChecksStatus) return;

    const previousStatus = watched.lastChecksStatus;
    watched.lastChecksStatus = currentStatus;

    if (currentStatus === "pass" && previousStatus !== "pass") {
      const checkNames = checks
        .filter((c) => c.status === "pass")
        .map((c) => c.name);

      this.emit("ci:passed", {
        prNumber: watched.prNumber,
        cardId: watched.cardId,
        checkNames,
      });
    } else if (currentStatus === "fail" && previousStatus !== "fail") {
      const failedCheck = checks.find((c) => c.status === "fail");
      this.emit("ci:failed", {
        prNumber: watched.prNumber,
        cardId: watched.cardId,
        failedCheckName: failedCheck?.name ?? "unknown",
        logUrl: failedCheck?.url ?? null,
      });
    }
  }

  private detectReviewTransitions(watched: WatchedPR, data: PRChecksData): void {
    if (!data.pr) return;

    // Detect review verdict changes
    const latestReview = getLatestReviewVerdict(data.pr.reviewers);
    if (latestReview && latestReview !== watched.lastReviewState) {
      watched.lastReviewState = latestReview;

      this.emit("review:submitted", {
        prNumber: watched.prNumber,
        cardId: watched.cardId,
        verdict: latestReview,
        commentBody: null,
      });
    }

    // Detect new inline comments
    const newComments = data.comments.filter(
      (c) => !watched.lastCommentIds.has(c.id),
    );

    if (newComments.length > 0) {
      // Update tracked comment IDs
      for (const c of newComments) {
        watched.lastCommentIds.add(c.id);
      }

      // Only emit if we had a previous state (skip initial load)
      if (watched.lastCommentIds.size > newComments.length) {
        this.emit("review:comments", {
          prNumber: watched.prNumber,
          cardId: watched.cardId,
          comments: newComments.map((c) => ({
            path: c.path ?? null,
            line: c.line ?? null,
            body: c.body,
          })),
        });
      }
    }
  }

  private handlePollFailure(): void {
    this.consecutiveFailures++;

    if (this.consecutiveFailures >= CONSECUTIVE_FAILURES_THRESHOLD) {
      const newInterval = Math.min(
        this.currentIntervalMs * 2,
        MAX_POLL_INTERVAL_MS,
      );

      if (newInterval !== this.currentIntervalMs) {
        this.restartPollingWithInterval(newInterval);

        if (!this.degraded) {
          this.degraded = true;
          this.emit("monitor:degraded", {
            reason: `${this.consecutiveFailures} consecutive poll failures`,
            newIntervalMs: newInterval,
          });
        }
      }
    }
  }

  private handlePollSuccess(): void {
    if (this.consecutiveFailures > 0) {
      this.consecutiveFailures = 0;

      if (this.degraded) {
        this.degraded = false;
        this.restartPollingWithInterval(this.baseIntervalMs);
        log.info("CIMonitor: recovered from degraded state, restored base interval");
      }
    }
  }
}

// ── Helpers ──────────────────────────────────────────────────────

function clampInterval(ms: number): number {
  if (!ms || ms < MIN_POLL_INTERVAL_MS) return MIN_POLL_INTERVAL_MS;
  if (ms > MAX_POLL_INTERVAL_MS) return MAX_POLL_INTERVAL_MS;
  return ms;
}

function deriveOverallCheckStatus(checks: CICheck[]): "pass" | "fail" | "pending" {
  const hasFailure = checks.some((c) => c.status === "fail");
  if (hasFailure) return "fail";

  const allPassed = checks.every((c) => c.status === "pass");
  if (allPassed) return "pass";

  return "pending";
}

function getLatestReviewVerdict(
  reviewers: Array<{ login: string; state: string }>,
): string | null {
  if (!reviewers || reviewers.length === 0) return null;

  // Find the most significant review state
  // Priority: CHANGES_REQUESTED > APPROVED > COMMENTED > PENDING
  const stateOrder: Record<string, number> = {
    CHANGES_REQUESTED: 4,
    APPROVED: 3,
    COMMENTED: 2,
    PENDING: 1,
  };

  let highestState: string | null = null;
  let highestPriority = 0;

  for (const reviewer of reviewers) {
    const state = reviewer.state.toUpperCase();
    const priority = stateOrder[state] ?? 0;
    if (priority > highestPriority) {
      highestPriority = priority;
      highestState = state.toLowerCase();
    }
  }

  return highestState;
}

export {
  clampInterval,
  deriveOverallCheckStatus,
  getLatestReviewVerdict,
  MIN_POLL_INTERVAL_MS as CI_MIN_POLL_INTERVAL_MS,
  MAX_POLL_INTERVAL_MS as CI_MAX_POLL_INTERVAL_MS,
  DEFAULT_POLL_INTERVAL_MS as CI_DEFAULT_POLL_INTERVAL_MS,
};
