/**
 * Transfer Resume Handler — encapsulates resume logic for interrupted
 * file transfers.
 *
 * Responsibilities:
 * - Persist resume state on transfer interruption (bytes transferred,
 *   remote mtime, remote size, partial SHA-256 checksum)
 * - On reconnect: verify remote file unchanged, verify local partial
 *   file integrity, resume from offset or restart
 * - Discard state after 3 failed resume attempts or 7-day expiry
 *
 * Composed into the FileTransferManager — does not own the SFTP channel
 * or transfer queue directly.
 */

import * as crypto from "node:crypto";
import * as fs from "node:fs";
import type { SFTPWrapper } from "ssh2";
import type { TransferHistoryStore } from "./ssh-transfer-history";
import type { TransferDirection, TransferResumeState } from "./ssh-types";

// ─── Types ───────────────────────────────────────────────────────────────────

export interface ResumeVerificationResult {
  canResume: boolean;
  offset: number;
  reason?: string;
}

export interface InterruptionContext {
  transferId: string;
  profileId: string;
  direction: TransferDirection;
  localPath: string;
  remotePath: string;
  bytesTransferred: number;
  totalBytes: number;
}

// ─── Constants ───────────────────────────────────────────────────────────────

const RESUME_EXPIRY_MS = 7 * 24 * 60 * 60 * 1000; // 7 days
const MAX_RESUME_ATTEMPTS = 3;

// ─── TransferResumeHandler ───────────────────────────────────────────────────

export class TransferResumeHandler {
  private resumeAttempts = new Map<string, number>();

  constructor(private readonly historyStore: TransferHistoryStore) {}

  /**
   * Persist resume state when a transfer is interrupted.
   *
   * Computes a SHA-256 checksum of the local partial file (for downloads)
   * or the bytes already transferred (for uploads), then stores the state
   * via TransferHistoryStore.
   */
  async persistInterruption(
    context: InterruptionContext,
    remoteMtime: number,
    remoteSize: number,
  ): Promise<void> {
    const { transferId, profileId, direction, localPath, remotePath, bytesTransferred, totalBytes } = context;

    if (bytesTransferred <= 0) return;

    const partialChecksum = await this.computePartialChecksum(localPath, bytesTransferred, direction);

    const now = Date.now();
    const state: TransferResumeState = {
      transferId,
      profileId,
      direction,
      localPath,
      remotePath,
      bytesTransferred,
      totalBytes,
      remoteMtime,
      remoteSize,
      partialChecksum,
      createdAt: now,
      expiresAt: now + RESUME_EXPIRY_MS,
    };

    this.historyStore.saveResumeState(state);
  }

  /**
   * Attempt to resume a transfer after reconnection.
   *
   * Checks for persisted resume state, verifies the remote file hasn't
   * changed (mtime + size), verifies local partial file integrity via
   * SHA-256, and returns the offset to resume from.
   *
   * Returns { canResume: false } if any verification fails or if the
   * maximum resume attempts have been exceeded.
   */
  async verifyAndPrepareResume(
    profileId: string,
    remotePath: string,
    localPath: string,
    direction: TransferDirection,
    sftp: SFTPWrapper,
  ): Promise<ResumeVerificationResult> {
    const state = this.historyStore.getResumeState(profileId, remotePath);

    if (!state) {
      return { canResume: false, offset: 0, reason: "no resume state found" };
    }

    // Check expiry (belt-and-suspenders — purgeExpired handles this too)
    if (Date.now() > state.expiresAt) {
      this.discardState(profileId, remotePath);
      return { canResume: false, offset: 0, reason: "resume state expired" };
    }

    // Check resume attempt count
    const attemptKey = this.attemptKey(profileId, remotePath);
    const attempts = this.resumeAttempts.get(attemptKey) ?? 0;
    if (attempts >= MAX_RESUME_ATTEMPTS) {
      this.discardState(profileId, remotePath);
      this.resumeAttempts.delete(attemptKey);
      return { canResume: false, offset: 0, reason: "exceeded maximum resume attempts (3)" };
    }

    // Verify remote file unchanged (for downloads, check source file mtime/size)
    if (direction === "download") {
      const remoteVerification = await this.verifyRemoteFile(sftp, remotePath, state);
      if (!remoteVerification.valid) {
        this.discardState(profileId, remotePath);
        return { canResume: false, offset: 0, reason: remoteVerification.reason };
      }
    }

    // Determine offset and verify local partial file integrity
    let offset: number;
    if (direction === "download") {
      const localVerification = await this.verifyLocalPartialFile(localPath, state);
      if (!localVerification.valid) {
        this.discardState(profileId, remotePath);
        this.deletePartialFile(localPath);
        return { canResume: false, offset: 0, reason: localVerification.reason };
      }
      offset = localVerification.size;
    } else {
      // Upload: query remote partial file size and verify it matches
      const remotePartialSize = await this.getRemoteFileSize(sftp, remotePath);
      if (remotePartialSize < 0) {
        // Remote file doesn't exist — restart
        this.discardState(profileId, remotePath);
        return { canResume: false, offset: 0, reason: "remote partial file not found" };
      }
      if (remotePartialSize !== state.bytesTransferred) {
        this.discardState(profileId, remotePath);
        return {
          canResume: false,
          offset: 0,
          reason: `remote partial size (${remotePartialSize}) does not match persisted offset (${state.bytesTransferred})`,
        };
      }
      offset = remotePartialSize;
    }

    return { canResume: true, offset };
  }

  /**
   * Mark a resume as successful — clears the attempt counter and
   * removes the persisted state.
   */
  onResumeSuccess(profileId: string, remotePath: string): void {
    const attemptKey = this.attemptKey(profileId, remotePath);
    this.resumeAttempts.delete(attemptKey);
    this.discardState(profileId, remotePath);
  }

  /**
   * Record a failed resume attempt. If the maximum is reached, the
   * state is discarded on the next verifyAndPrepareResume call.
   */
  onResumeFailure(profileId: string, remotePath: string): void {
    const attemptKey = this.attemptKey(profileId, remotePath);
    const attempts = this.resumeAttempts.get(attemptKey) ?? 0;
    this.resumeAttempts.set(attemptKey, attempts + 1);
  }

  /**
   * Check whether a resume state exists for a given transfer.
   */
  hasResumeState(profileId: string, remotePath: string): boolean {
    return this.historyStore.getResumeState(profileId, remotePath) !== null;
  }

  // ─── Private ─────────────────────────────────────────────────────────────

  private async verifyRemoteFile(
    sftp: SFTPWrapper,
    remotePath: string,
    state: TransferResumeState,
  ): Promise<{ valid: boolean; reason?: string }> {
    try {
      const stats = await this.sftpStat(sftp, remotePath);

      if (state.direction === "download") {
        // For downloads, the source remote file must be unchanged
        if (stats.mtime !== state.remoteMtime) {
          return { valid: false, reason: "remote file mtime changed since interruption" };
        }
        if (stats.size !== state.remoteSize) {
          return { valid: false, reason: "remote file size changed since interruption" };
        }
      }
      // For uploads, we only need the remote partial file to exist with correct size
      // (handled in verifyAndPrepareResume)

      return { valid: true };
    } catch {
      return { valid: false, reason: "failed to stat remote file" };
    }
  }

  private async verifyLocalPartialFile(
    localPath: string,
    state: TransferResumeState,
  ): Promise<{ valid: boolean; size: number; reason?: string }> {
    try {
      const stats = fs.statSync(localPath);
      const localSize = stats.size;

      if (localSize === 0) {
        return { valid: false, size: 0, reason: "local partial file is empty" };
      }

      if (localSize > state.bytesTransferred) {
        return {
          valid: false,
          size: localSize,
          reason: "local file is larger than persisted bytes transferred",
        };
      }

      // Verify SHA-256 checksum of the partial content
      const checksum = await this.computePartialChecksum(localPath, localSize, "download");
      if (checksum !== state.partialChecksum) {
        return { valid: false, size: localSize, reason: "partial file checksum mismatch" };
      }

      return { valid: true, size: localSize };
    } catch {
      return { valid: false, size: 0, reason: "local partial file not found or unreadable" };
    }
  }

  private async computePartialChecksum(
    localPath: string,
    byteCount: number,
    direction: TransferDirection,
  ): Promise<string> {
    return new Promise((resolve, reject) => {
      const hash = crypto.createHash("sha256");
      const stream = fs.createReadStream(localPath, { start: 0, end: byteCount - 1 });

      stream.on("data", (chunk) => hash.update(chunk));
      stream.on("end", () => resolve(hash.digest("hex")));
      stream.on("error", (err) => {
        // If the file doesn't exist or is unreadable, return empty checksum
        if (direction === "upload" && (err as NodeJS.ErrnoException).code === "ENOENT") {
          resolve("");
        } else {
          reject(err);
        }
      });
    });
  }

  private async getRemoteFileSize(sftp: SFTPWrapper, remotePath: string): Promise<number> {
    try {
      const stats = await this.sftpStat(sftp, remotePath);
      return stats.size;
    } catch {
      return -1;
    }
  }

  private sftpStat(sftp: SFTPWrapper, remotePath: string): Promise<{ size: number; mtime: number }> {
    return new Promise((resolve, reject) => {
      sftp.stat(remotePath, (err, stats) => {
        if (err) {
          reject(err);
        } else {
          resolve({
            size: stats.size,
            mtime: stats.mtime,
          });
        }
      });
    });
  }

  private discardState(profileId: string, remotePath: string): void {
    // Retrieve the existing state to get its transferId, then overwrite
    // it with an expired entry using the same id (INSERT OR REPLACE by PK).
    const existing = this.historyStore.getResumeState(profileId, remotePath);
    const id = existing?.transferId ?? `discarded-${crypto.randomUUID()}`;

    this.historyStore.saveResumeState({
      transferId: id,
      profileId,
      direction: existing?.direction ?? "download",
      localPath: existing?.localPath ?? "",
      remotePath,
      bytesTransferred: 0,
      totalBytes: 0,
      remoteMtime: 0,
      remoteSize: 0,
      partialChecksum: "",
      createdAt: Date.now(),
      expiresAt: 0, // already expired — will be purged
    });
  }

  private deletePartialFile(localPath: string): void {
    try {
      fs.unlinkSync(localPath);
    } catch {
      // Best-effort deletion — file may not exist
    }
  }

  private attemptKey(profileId: string, remotePath: string): string {
    return `${profileId}::${remotePath}`;
  }
}
