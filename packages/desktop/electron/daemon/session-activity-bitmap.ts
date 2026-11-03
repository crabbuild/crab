/**
 * SessionActivityBitmap — lightweight per-session dirty-bit tracker.
 *
 * Implements SessionActivityTracker for CheckpointTimer. Each session
 * has a single boolean flag that is set by onSessionData and cleared
 * after each checkpoint cycle.
 */

import type { SessionActivityTracker } from "./checkpoint-timer";

// ---------------------------------------------------------------------------
// SessionActivityBitmap
// ---------------------------------------------------------------------------

export class SessionActivityBitmap implements SessionActivityTracker {
  private bits: Map<string, boolean> = new Map();

  /**
   * Register a new session. Starts with no activity.
   */
  register(id: string): void {
    this.bits.set(id, false);
  }

  /**
   * Mark a session as having received new data since the last checkpoint.
   */
  markDirty(id: string): void {
    this.bits.set(id, true);
  }

  /**
   * Returns true if the session has received data since the last checkpoint.
   */
  hasActivity(id: string): boolean {
    return this.bits.get(id) === true;
  }

  /**
   * Clear the activity flag for a session (called after checkpoint).
   */
  clearActivity(id: string): void {
    this.bits.set(id, false);
  }

  /**
   * Returns all session IDs currently tracked.
   */
  getTrackedSessionIds(): string[] {
    return [...this.bits.keys()];
  }

  /**
   * Remove a session from tracking (called on session exit).
   */
  remove(id: string): void {
    this.bits.delete(id);
  }
}
