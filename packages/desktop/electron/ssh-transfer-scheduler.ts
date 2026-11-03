/**
 * Transfer Scheduler — holds transfers in "scheduled" state until their
 * start time, then moves them to the FileTransferManager queue.
 *
 * Persists scheduled transfers to SQLite so they survive app restarts.
 * Fires within 30 seconds of the scheduled time. If the SSH connection
 * is not active at fire time, retries every 30 seconds for up to 5 minutes
 * before marking the transfer as failed.
 */

import { EventEmitter } from "node:events";
import type Database from "better-sqlite3";
import type { FileTransferManager } from "./ssh-file-transfer";
import type { SshSessionManager } from "./ssh-manager";
import type { ScheduledTransfer } from "./ssh-types";

// ── Constants ────────────────────────────────────────────────────

const MAX_SCHEDULED_PER_PROFILE = 50;
const DISPATCH_TOLERANCE_MS = 30_000;
const RETRY_INTERVAL_MS = 30_000;
const MAX_RETRY_DURATION_MS = 5 * 60 * 1000;
const TIMER_CHECK_INTERVAL_MS = 10_000;

// ── Internal state for a pending timer ───────────────────────────

interface PendingTimer {
  timer: NodeJS.Timeout;
  retryStartedAt?: number;
  retryTimer?: NodeJS.Timeout;
}

// ── TransferScheduler ────────────────────────────────────────────

export class TransferScheduler extends EventEmitter {
  private ftm: FileTransferManager;
  private sessionManager: SshSessionManager;
  private db: Database.Database;
  private timers = new Map<string, PendingTimer>();
  private checkInterval: NodeJS.Timeout | null = null;

  constructor(
    ftm: FileTransferManager,
    sessionManager: SshSessionManager,
    db: Database.Database,
  ) {
    super();
    this.ftm = ftm;
    this.sessionManager = sessionManager;
    this.db = db;

    this.restoreTimers();
    this.startCheckInterval();
  }

  /**
   * Schedule a transfer for a future time. Persists to SQLite and sets
   * a dispatch timer. Returns the generated transfer ID.
   *
   * Throws if the scheduled time is in the past or the profile already
   * has 50 scheduled transfers.
   */
  schedule(transfer: Omit<ScheduledTransfer, "id" | "createdAt">): string {
    const now = Date.now();

    if (transfer.scheduledAt <= now) {
      throw new Error("Scheduled time must be in the future");
    }

    const count = this.countPendingForProfile(transfer.profileId);
    if (count >= MAX_SCHEDULED_PER_PROFILE) {
      throw new Error(
        `Maximum of ${MAX_SCHEDULED_PER_PROFILE} scheduled transfers per profile reached`,
      );
    }

    const id = crypto.randomUUID();
    const createdAt = now;

    const stmt = this.db.prepare(`
      INSERT INTO scheduled_transfers (id, profile_id, direction, local_path, remote_path, scheduled_at, created_at, status)
      VALUES (?, ?, ?, ?, ?, ?, ?, 'pending')
    `);
    stmt.run(id, transfer.profileId, transfer.direction, transfer.localPath, transfer.remotePath, transfer.scheduledAt, createdAt);

    this.setTimer(id, transfer.profileId, transfer.direction, transfer.localPath, transfer.remotePath, transfer.scheduledAt);

    return id;
  }

  /**
   * Cancel a scheduled transfer. Removes from the database and clears
   * any pending timer. Returns true if the transfer was found and cancelled.
   */
  cancel(transferId: string): boolean {
    const row = this.db.prepare(
      `SELECT id FROM scheduled_transfers WHERE id = ? AND status = 'pending'`,
    ).get(transferId) as { id: string } | undefined;

    if (!row) {
      return false;
    }

    this.db.prepare(
      `UPDATE scheduled_transfers SET status = 'cancelled' WHERE id = ?`,
    ).run(transferId);

    this.clearTimer(transferId);
    return true;
  }

  /**
   * List all pending scheduled transfers for a profile.
   */
  listScheduled(profileId: string): ScheduledTransfer[] {
    const rows = this.db.prepare(`
      SELECT id, profile_id, direction, local_path, remote_path, scheduled_at, created_at
      FROM scheduled_transfers
      WHERE profile_id = ? AND status = 'pending'
      ORDER BY scheduled_at ASC
    `).all(profileId) as Array<{
      id: string;
      profile_id: string;
      direction: "upload" | "download";
      local_path: string;
      remote_path: string;
      scheduled_at: number;
      created_at: number;
    }>;

    return rows.map((row) => ({
      id: row.id,
      profileId: row.profile_id,
      direction: row.direction,
      localPath: row.local_path,
      remotePath: row.remote_path,
      scheduledAt: row.scheduled_at,
      createdAt: row.created_at,
    }));
  }

  /**
   * Stop all timers and the check interval. Call on app shutdown.
   */
  dispose(): void {
    if (this.checkInterval) {
      clearInterval(this.checkInterval);
      this.checkInterval = null;
    }

    for (const [, pending] of this.timers) {
      clearTimeout(pending.timer);
      if (pending.retryTimer) {
        clearTimeout(pending.retryTimer);
      }
    }
    this.timers.clear();
  }

  // ── Private ────────────────────────────────────────────────────

  private countPendingForProfile(profileId: string): number {
    const row = this.db.prepare(
      `SELECT COUNT(*) as count FROM scheduled_transfers WHERE profile_id = ? AND status = 'pending'`,
    ).get(profileId) as { count: number };
    return row.count;
  }

  private setTimer(
    id: string,
    profileId: string,
    direction: "upload" | "download",
    localPath: string,
    remotePath: string,
    scheduledAt: number,
  ): void {
    const now = Date.now();
    const delay = Math.max(0, scheduledAt - now - DISPATCH_TOLERANCE_MS);

    const timer = setTimeout(() => {
      this.fireTransfer(id, profileId, direction, localPath, remotePath);
    }, delay);

    this.timers.set(id, { timer });
  }

  private clearTimer(transferId: string): void {
    const pending = this.timers.get(transferId);
    if (pending) {
      clearTimeout(pending.timer);
      if (pending.retryTimer) {
        clearTimeout(pending.retryTimer);
      }
      this.timers.delete(transferId);
    }
  }

  private async fireTransfer(
    id: string,
    profileId: string,
    direction: "upload" | "download",
    localPath: string,
    remotePath: string,
  ): Promise<void> {
    // Verify the transfer is still pending
    const row = this.db.prepare(
      `SELECT status FROM scheduled_transfers WHERE id = ?`,
    ).get(id) as { status: string } | undefined;

    if (!row || row.status !== "pending") {
      this.timers.delete(id);
      return;
    }

    const isConnected = this.isConnectionActive(profileId);

    if (isConnected) {
      await this.executeScheduledTransfer(id, profileId, direction, localPath, remotePath);
    } else {
      this.startRetryLoop(id, profileId, direction, localPath, remotePath);
    }
  }

  private async executeScheduledTransfer(
    id: string,
    profileId: string,
    direction: "upload" | "download",
    localPath: string,
    remotePath: string,
  ): Promise<void> {
    this.db.prepare(
      `UPDATE scheduled_transfers SET status = 'executing' WHERE id = ?`,
    ).run(id);

    try {
      if (direction === "upload") {
        this.ftm.upload(profileId, localPath, remotePath);
      } else {
        this.ftm.download(profileId, remotePath, localPath);
      }

      this.db.prepare(
        `UPDATE scheduled_transfers SET status = 'completed' WHERE id = ?`,
      ).run(id);

      this.timers.delete(id);
      this.emit("scheduled-fired", { transferId: id, result: "completed" });
    } catch (err) {
      const error = err instanceof Error ? err.message : String(err);

      this.db.prepare(
        `UPDATE scheduled_transfers SET status = 'failed', error = ? WHERE id = ?`,
      ).run(error, id);

      this.timers.delete(id);
      this.emit("scheduled-fired", { transferId: id, result: "failed", error });
    }
  }

  private startRetryLoop(
    id: string,
    profileId: string,
    direction: "upload" | "download",
    localPath: string,
    remotePath: string,
  ): void {
    const pending = this.timers.get(id);
    if (!pending) return;

    pending.retryStartedAt = Date.now();
    this.scheduleRetry(id, profileId, direction, localPath, remotePath);
  }

  private scheduleRetry(
    id: string,
    profileId: string,
    direction: "upload" | "download",
    localPath: string,
    remotePath: string,
  ): void {
    const pending = this.timers.get(id);
    if (!pending || pending.retryStartedAt === undefined) return;

    const elapsed = Date.now() - pending.retryStartedAt;
    if (elapsed >= MAX_RETRY_DURATION_MS) {
      // Exceeded 5-minute retry window — mark as failed
      const error = "SSH connection not active after 5 minutes of retries";

      this.db.prepare(
        `UPDATE scheduled_transfers SET status = 'failed', error = ? WHERE id = ?`,
      ).run(error, id);

      this.timers.delete(id);
      this.emit("scheduled-fired", { transferId: id, result: "failed", error });
      return;
    }

    pending.retryTimer = setTimeout(async () => {
      // Verify still pending
      const row = this.db.prepare(
        `SELECT status FROM scheduled_transfers WHERE id = ?`,
      ).get(id) as { status: string } | undefined;

      if (!row || row.status !== "pending") {
        this.timers.delete(id);
        return;
      }

      if (this.isConnectionActive(profileId)) {
        await this.executeScheduledTransfer(id, profileId, direction, localPath, remotePath);
      } else {
        this.scheduleRetry(id, profileId, direction, localPath, remotePath);
      }
    }, RETRY_INTERVAL_MS);
  }

  private isConnectionActive(profileId: string): boolean {
    const status = this.sessionManager.getStatus(profileId);
    return status === "connected";
  }

  /**
   * On startup, restore timers for any pending scheduled transfers
   * that haven't fired yet.
   */
  private restoreTimers(): void {
    const rows = this.db.prepare(`
      SELECT id, profile_id, direction, local_path, remote_path, scheduled_at
      FROM scheduled_transfers
      WHERE status = 'pending'
    `).all() as Array<{
      id: string;
      profile_id: string;
      direction: "upload" | "download";
      local_path: string;
      remote_path: string;
      scheduled_at: number;
    }>;

    const now = Date.now();

    for (const row of rows) {
      if (row.scheduled_at <= now) {
        // Already past due — fire immediately
        this.fireTransfer(row.id, row.profile_id, row.direction, row.local_path, row.remote_path);
      } else {
        this.setTimer(row.id, row.profile_id, row.direction, row.local_path, row.remote_path, row.scheduled_at);
      }
    }
  }

  /**
   * Periodic check to catch any timers that may have drifted or been
   * missed (e.g., after system sleep). Fires transfers that are past
   * due but still in pending state.
   */
  private startCheckInterval(): void {
    this.checkInterval = setInterval(() => {
      const now = Date.now();
      const rows = this.db.prepare(`
        SELECT id, profile_id, direction, local_path, remote_path, scheduled_at
        FROM scheduled_transfers
        WHERE status = 'pending' AND scheduled_at <= ?
      `).all(now) as Array<{
        id: string;
        profile_id: string;
        direction: "upload" | "download";
        local_path: string;
        remote_path: string;
        scheduled_at: number;
      }>;

      for (const row of rows) {
        // Only fire if we don't already have a timer/retry in progress
        if (!this.timers.has(row.id)) {
          this.fireTransfer(row.id, row.profile_id, row.direction, row.local_path, row.remote_path);
        }
      }
    }, TIMER_CHECK_INTERVAL_MS);
  }
}
