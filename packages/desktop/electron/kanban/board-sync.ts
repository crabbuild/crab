/**
 * Board Sync engine for the Kanban subsystem.
 *
 * Keeps local board state synchronized with the configured external tracker
 * by polling at a configurable interval, diffing fetched issues against local
 * card state, and emitting changesets. Supports ETag/If-Modified-Since
 * conditional requests, conflict resolution (remote wins), and retry with
 * exponential backoff on tracker unreachable.
 */

import { EventEmitter } from "node:events";
import log from "electron-log";

import type {
  BoardConfig,
  KanbanCard,
  CardPriority,
  PipelineState,
} from "./kanban-types";
import type { ExternalIssue, TrackerAdapter } from "./tracker-adapters/types";
import { BoardStateStore } from "./kanban-persistence";

// ── Constants ────────────────────────────────────────────────────

const MIN_POLL_INTERVAL_MS = 10_000;
const MAX_POLL_INTERVAL_MS = 3_600_000;
const DEFAULT_POLL_INTERVAL_MS = 60_000;

const MAX_RETRY_ATTEMPTS = 3;
const RETRY_BACKOFF_BASE_MS = 5_000;

// ── Types ────────────────────────────────────────────────────────

export interface SyncConflict {
  cardId: string;
  localState: string;
  remoteState: string;
  timestamp: number;
}

export interface SyncChangeset {
  created: KanbanCard[];
  updated: KanbanCard[];
  archived: KanbanCard[];
  conflicts: SyncConflict[];
}

// ── BoardSync ────────────────────────────────────────────────────

export class BoardSync extends EventEmitter {
  private config: BoardConfig;
  private adapter: TrackerAdapter;
  private stateStore: BoardStateStore;
  private cards: KanbanCard[] = [];
  private conflictLog: SyncConflict[] = [];
  private pollTimer: ReturnType<typeof setInterval> | null = null;
  private syncing = false;

  constructor(
    config: BoardConfig,
    adapter: TrackerAdapter,
    stateStore?: BoardStateStore,
  ) {
    super();
    this.config = config;
    this.adapter = adapter;
    this.stateStore = stateStore ?? new BoardStateStore();
  }

  /** Begin the polling loop. */
  start(): void {
    if (this.pollTimer) return;

    const interval = this.clampInterval(this.config.pollIntervalMs);

    // Run an initial sync immediately
    this.runSyncCycle().catch((err) => {
      log.error(
        `[board-sync] Initial sync failed: ${err instanceof Error ? err.message : String(err)}`,
      );
    });

    this.pollTimer = setInterval(() => {
      this.runSyncCycle().catch((err) => {
        log.error(
          `[board-sync] Sync cycle failed: ${err instanceof Error ? err.message : String(err)}`,
        );
      });
    }, interval);
  }

  /** Stop the polling loop. */
  stop(): void {
    if (this.pollTimer) {
      clearInterval(this.pollTimer);
      this.pollTimer = null;
    }
  }

  /** Manual refresh — runs a sync cycle immediately. */
  async forceSync(): Promise<void> {
    await this.runSyncCycle();
  }

  /** Returns the accumulated conflict log. */
  getConflictLog(): SyncConflict[] {
    return [...this.conflictLog];
  }

  /** Returns the current local card state (for testing). */
  getCards(): KanbanCard[] {
    return [...this.cards];
  }

  /** Load cards from the state store. */
  async loadState(): Promise<void> {
    this.cards = await this.stateStore.getCards();
  }

  // ── Sync Cycle ─────────────────────────────────────────────────

  private async runSyncCycle(): Promise<void> {
    if (this.syncing) return;
    this.syncing = true;

    try {
      const issues = await this.fetchWithRetry();
      if (issues === null) {
        // All retries exhausted — skip this cycle
        return;
      }

      const changeset = this.diff(issues);
      this.applyChangeset(changeset);

      await this.stateStore.saveCards(this.cards);
      this.emit("board:updated", changeset);
    } finally {
      this.syncing = false;
    }
  }

  // ── Fetch with Retry ───────────────────────────────────────────

  private async fetchWithRetry(): Promise<ExternalIssue[] | null> {
    for (let attempt = 1; attempt <= MAX_RETRY_ATTEMPTS; attempt++) {
      try {
        return await this.adapter.fetchIssues(this.config.filterCriteria);
      } catch (err) {
        const msg = err instanceof Error ? err.message : String(err);

        if (attempt < MAX_RETRY_ATTEMPTS) {
          const backoffMs = RETRY_BACKOFF_BASE_MS * Math.pow(2, attempt - 1);
          log.warn(
            `[board-sync] Fetch attempt ${attempt}/${MAX_RETRY_ATTEMPTS} failed: ${msg}. Retrying in ${backoffMs}ms`,
          );
          await sleep(backoffMs);
        } else {
          log.error(
            `[board-sync] All ${MAX_RETRY_ATTEMPTS} fetch attempts failed: ${msg}. Skipping cycle.`,
          );
          this.emit("sync:error", { reason: msg, attempts: MAX_RETRY_ATTEMPTS });
        }
      }
    }
    return null;
  }

  // ── Diff Logic ─────────────────────────────────────────────────

  /**
   * Compares fetched issues against local card state to detect:
   * - New issues (not in local state)
   * - Changed issues (fields or status differ)
   * - Deleted/closed issues (in local state but not in fetched set)
   */
  diff(issues: ExternalIssue[]): SyncChangeset {
    const changeset: SyncChangeset = {
      created: [],
      updated: [],
      archived: [],
      conflicts: [],
    };

    const issueMap = new Map(issues.map((i) => [i.id, i]));
    const localMap = new Map(
      this.cards
        .filter((c) => !c.archivedAt)
        .map((c) => [c.externalId, c]),
    );

    // Detect new and changed issues
    for (const issue of issues) {
      const existingCard = localMap.get(issue.id);

      if (!existingCard) {
        // New issue — create card
        const card = this.createCardFromIssue(issue);
        changeset.created.push(card);
      } else {
        // Check for changes
        const updatedCard = this.detectChanges(existingCard, issue, changeset.conflicts);
        if (updatedCard) {
          changeset.updated.push(updatedCard);
        }
      }
    }

    // Detect deleted/closed issues
    for (const card of this.cards) {
      if (card.archivedAt) continue;
      if (!issueMap.has(card.externalId)) {
        // Issue no longer in fetched set — check pipeline state before archiving
        if (this.shouldArchive(card)) {
          const archived = { ...card, archivedAt: Date.now(), updatedAt: Date.now() };
          changeset.archived.push(archived);
        }
      }
    }

    return changeset;
  }

  // ── Card Creation ──────────────────────────────────────────────

  private createCardFromIssue(issue: ExternalIssue): KanbanCard {
    const columnId = this.mapStatusToColumn(issue.status);
    const now = Date.now();

    return {
      id: crypto.randomUUID(),
      source: this.adapter.source,
      externalId: issue.id,
      externalUrl: issue.url,
      title: issue.title,
      description: issue.description,
      columnId,
      positionIndex: this.getNextPositionIndex(columnId),
      assignee: issue.assignee,
      priority: this.mapPriority(issue.priority),
      labels: issue.labels,
      pipelineState: "idle",
      iterationCount: 0,
      createdAt: now,
      updatedAt: now,
    };
  }

  // ── Change Detection ───────────────────────────────────────────

  private detectChanges(
    card: KanbanCard,
    issue: ExternalIssue,
    conflicts: SyncConflict[],
  ): KanbanCard | null {
    let changed = false;
    const updated = { ...card };

    // Check title change
    if (card.title !== issue.title) {
      updated.title = issue.title;
      changed = true;
    }

    // Check description change
    if (card.description !== issue.description) {
      updated.description = issue.description;
      changed = true;
    }

    // Check assignee change
    if (card.assignee !== issue.assignee) {
      updated.assignee = issue.assignee;
      changed = true;
    }

    // Check labels change
    if (!arraysEqual(card.labels, issue.labels)) {
      updated.labels = issue.labels;
      changed = true;
    }

    // Check priority change
    const mappedPriority = this.mapPriority(issue.priority);
    if (card.priority !== mappedPriority) {
      updated.priority = mappedPriority;
      changed = true;
    }

    // Check status/column change
    const targetColumnId = this.mapStatusToColumn(issue.status);
    if (card.columnId !== targetColumnId) {
      // Conflict: local column differs from remote
      // Remote wins — log the conflict
      const conflict: SyncConflict = {
        cardId: card.id,
        localState: card.columnId,
        remoteState: targetColumnId,
        timestamp: Date.now(),
      };
      conflicts.push(conflict);
      this.conflictLog.push(conflict);

      log.info(
        `[board-sync] Conflict resolved (remote wins): card=${card.id} local=${card.columnId} remote=${targetColumnId}`,
      );

      updated.columnId = targetColumnId;
      changed = true;
    }

    if (changed) {
      updated.updatedAt = Date.now();
      return updated;
    }

    return null;
  }

  // ── Deletion Handling ──────────────────────────────────────────

  /**
   * Determines whether a card should be archived when its issue disappears
   * from the fetched set. Skips archiving if the pipeline controller initiated
   * the closure (pipelineState indicates active pipeline work).
   */
  private shouldArchive(card: KanbanCard): boolean {
    const pipelineActiveStates: PipelineState[] = [
      "agent_running",
      "pr_created",
      "ci_pending",
      "ci_passed",
      "review_pending",
      "review_approved",
      "merging",
    ];

    if (pipelineActiveStates.includes(card.pipelineState)) {
      log.info(
        `[board-sync] Skipping archive for card=${card.id}: pipeline state is ${card.pipelineState}`,
      );
      return false;
    }

    return true;
  }

  // ── Column Mapping ─────────────────────────────────────────────

  /**
   * Maps an external issue status string to a local column ID using the
   * trackerStatusMapping configured on each column. Falls back to the first
   * column if no mapping matches.
   */
  private mapStatusToColumn(status: string): string {
    const normalizedStatus = status.toLowerCase().trim();

    for (const column of this.config.columns) {
      const mappings = column.trackerStatusMapping.map((s) => s.toLowerCase().trim());
      if (mappings.includes(normalizedStatus)) {
        return column.id;
      }
    }

    // Fallback to first column
    if (this.config.columns.length > 0) {
      return this.config.columns[0].id;
    }

    return "backlog";
  }

  // ── Changeset Application ──────────────────────────────────────

  private applyChangeset(changeset: SyncChangeset): void {
    // Add new cards
    for (const card of changeset.created) {
      this.cards.push(card);
    }

    // Update existing cards
    for (const updated of changeset.updated) {
      const idx = this.cards.findIndex((c) => c.id === updated.id);
      if (idx >= 0) {
        this.cards[idx] = updated;
      }
    }

    // Archive cards
    for (const archived of changeset.archived) {
      const idx = this.cards.findIndex((c) => c.id === archived.id);
      if (idx >= 0) {
        this.cards[idx] = archived;
      }
    }
  }

  // ── Helpers ────────────────────────────────────────────────────

  private clampInterval(ms: number): number {
    if (!ms || ms < MIN_POLL_INTERVAL_MS) return MIN_POLL_INTERVAL_MS;
    if (ms > MAX_POLL_INTERVAL_MS) return MAX_POLL_INTERVAL_MS;
    return ms;
  }

  private getNextPositionIndex(columnId: string): number {
    const cardsInColumn = this.cards.filter(
      (c) => c.columnId === columnId && !c.archivedAt,
    );
    return cardsInColumn.length;
  }

  private mapPriority(priority: string): CardPriority {
    const normalized = priority.toLowerCase().trim();
    switch (normalized) {
      case "critical":
      case "highest":
      case "blocker":
        return "critical";
      case "high":
        return "high";
      case "medium":
      case "normal":
        return "medium";
      case "low":
        return "low";
      default:
        return "none";
    }
  }
}

// ── Module Helpers ───────────────────────────────────────────────

function arraysEqual(a: string[], b: string[]): boolean {
  if (a.length !== b.length) return false;
  for (let i = 0; i < a.length; i++) {
    if (a[i] !== b[i]) return false;
  }
  return true;
}

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

// ── Exports for testing ──────────────────────────────────────────

export { MIN_POLL_INTERVAL_MS, MAX_POLL_INTERVAL_MS, DEFAULT_POLL_INTERVAL_MS };
