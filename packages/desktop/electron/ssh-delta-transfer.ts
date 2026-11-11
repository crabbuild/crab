/**
 * Delta Transfer Handler — integrates the DeltaSyncEngine with file
 * transfer operations, providing delta-based transfers for large files
 * and falling back to full transfers when delta is unavailable or fails.
 *
 * For files >= 1 MB: attempts delta sync via DeltaSyncEngine.computeDelta(),
 * applies delta upload/download if the plan doesn't indicate full fallback,
 * and verifies integrity after transfer. Falls back to full transfer on
 * any failure, timeout, or integrity mismatch.
 *
 * For files < 1 MB: skips delta entirely, delegates to full transfer.
 *
 * Checks the delta RPC handlers before attempting
 * any delta operation.
 */

import * as fs from "node:fs";
import type { DeltaSyncEngine, DeltaPlan } from "./ssh-delta-sync";
import type { CapabilityNegotiator } from "./ssh-capabilities";

// ── Constants ────────────────────────────────────────────────────

/** Files smaller than this skip delta entirely. */
const DELTA_SIZE_THRESHOLD = 1 * 1024 * 1024; // 1 MB

/** Maximum time for the entire delta transfer operation before fallback. */
const DELTA_TRANSFER_TIMEOUT_MS = 60_000;

// ── Types ────────────────────────────────────────────────────────

export type TransferDirection = "upload" | "download";

export interface DeltaTransferResult {
  usedDelta: boolean;
  savingsPercent: number;
  fallbackReason?: string;
}

export interface Logger {
  warn(message: string, context?: Record<string, unknown>): void;
  debug?(message: string, context?: Record<string, unknown>): void;
}

export interface DeltaTransferHandlerOptions {
  logger?: Logger;
  /** Override for testing — returns file size in bytes, or -1 if file doesn't exist. */
  getFileSize?: (filePath: string) => number;
}

// ── DeltaTransferHandler ─────────────────────────────────────────

export class DeltaTransferHandler {
  private readonly deltaSyncEngine: DeltaSyncEngine;
  private readonly capabilityNegotiator: CapabilityNegotiator;
  private readonly logger: Logger;
  private readonly getFileSizeFn: (filePath: string) => number;

  constructor(
    deltaSyncEngine: DeltaSyncEngine,
    capabilityNegotiator: CapabilityNegotiator,
    options?: DeltaTransferHandlerOptions,
  ) {
    this.deltaSyncEngine = deltaSyncEngine;
    this.capabilityNegotiator = capabilityNegotiator;
    this.logger = options?.logger ?? { warn: () => {} };
    this.getFileSizeFn = options?.getFileSize ?? DeltaTransferHandler.defaultGetFileSize;
  }

  private static defaultGetFileSize(filePath: string): number {
    try {
      return fs.statSync(filePath).size;
    } catch {
      return -1;
    }
  }

  /**
   * Attempt a delta-based file transfer. Returns a result indicating
   * whether delta was used and the savings achieved.
   *
   * If delta cannot be used (file too small, capability unsupported,
   * delta computation fails, integrity check fails, or timeout), the
   * result indicates fallback with a reason. The caller is responsible
   * for performing the full transfer when usedDelta is false.
   */
  async transfer(
    profileId: string,
    localPath: string,
    remotePath: string,
    direction: TransferDirection,
  ): Promise<DeltaTransferResult> {
    // Check capability support
    if (!this.capabilityNegotiator.isSupported(profileId, "chunk_signatures", "write_chunks", "read_chunks")) {
      return {
        usedDelta: false,
        savingsPercent: 0,
        fallbackReason: "deltaSync capability not supported by remote agent",
      };
    }

    // Check file size threshold
    const fileSize = this.getFileSize(localPath, direction);
    if (fileSize < DELTA_SIZE_THRESHOLD) {
      return {
        usedDelta: false,
        savingsPercent: 0,
        fallbackReason: "file size below 1 MB threshold",
      };
    }

    // Attempt delta transfer with timeout
    try {
      return await this.withTimeout(
        this.executeDeltaTransfer(profileId, localPath, remotePath, direction),
        DELTA_TRANSFER_TIMEOUT_MS,
      );
    } catch (err) {
      const reason = err instanceof Error ? err.message : String(err);
      this.logger.warn("Delta transfer failed, falling back to full transfer", {
        profileId,
        localPath,
        remotePath,
        direction,
        reason,
      });
      return {
        usedDelta: false,
        savingsPercent: 0,
        fallbackReason: reason,
      };
    }
  }

  /**
   * Check whether delta transfer should be attempted for a given file.
   * Useful for pre-flight checks without performing the transfer.
   */
  shouldAttemptDelta(profileId: string, localPath: string, direction: TransferDirection): boolean {
    if (!this.capabilityNegotiator.isSupported(profileId, "chunk_signatures", "write_chunks", "read_chunks")) {
      return false;
    }

    const fileSize = this.getFileSize(localPath, direction);
    return fileSize >= DELTA_SIZE_THRESHOLD;
  }

  // ── Private ────────────────────────────────────────────────────

  /**
   * Execute the delta transfer: compute plan, apply delta, verify integrity.
   */
  private async executeDeltaTransfer(
    profileId: string,
    localPath: string,
    remotePath: string,
    direction: TransferDirection,
  ): Promise<DeltaTransferResult> {
    // Compute delta plan
    const plan = await this.deltaSyncEngine.computeDelta(profileId, localPath, remotePath);

    // If the engine indicates full fallback, bail out
    if (plan.fallbackToFull) {
      return {
        usedDelta: false,
        savingsPercent: 0,
        fallbackReason: "delta plan indicates full transfer required",
      };
    }

    // If no chunks differ, the files are identical — nothing to transfer
    if (plan.newChunks.length === 0 && plan.matchedChunks > 0) {
      return {
        usedDelta: true,
        savingsPercent: 100,
      };
    }

    // Apply delta based on direction
    if (direction === "upload") {
      await this.deltaSyncEngine.applyDeltaUpload(profileId, plan, localPath, remotePath);
    } else {
      await this.deltaSyncEngine.applyDeltaDownload(profileId, plan, localPath, remotePath);
    }

    // Verify integrity after transfer
    const expectedHash = direction === "upload" ? plan.sourceHash : plan.targetHash;
    const verifyPath = direction === "upload" ? remotePath : localPath;

    // For uploads, we verify the source hash matches what we sent.
    // For downloads, we verify the local file matches the expected target hash.
    const integrityOk = await this.verifyAfterTransfer(
      direction,
      localPath,
      plan,
    );

    if (!integrityOk) {
      this.logger.warn("Delta transfer integrity check failed, falling back to full transfer", {
        profileId,
        localPath,
        remotePath,
        direction,
        expectedHash,
        verifyPath,
      });
      return {
        usedDelta: false,
        savingsPercent: 0,
        fallbackReason: "integrity verification failed after delta transfer",
      };
    }

    return {
      usedDelta: true,
      savingsPercent: plan.totalSavingsPercent,
    };
  }

  /**
   * Verify file integrity after delta transfer.
   * For downloads: verify the local file matches the expected hash.
   * For uploads: the remote agent verifies via write_chunks RPC response,
   * so we verify the source file hash matches what we computed.
   */
  private async verifyAfterTransfer(
    direction: TransferDirection,
    localPath: string,
    plan: DeltaPlan,
  ): Promise<boolean> {
    if (direction === "download") {
      // Verify the reconstructed local file matches the target hash
      return this.deltaSyncEngine.verifyIntegrity(localPath, plan.targetHash);
    }

    // For uploads, verify the source file still matches what we computed
    // (guards against local file modification during transfer)
    return this.deltaSyncEngine.verifyIntegrity(localPath, plan.sourceHash);
  }

  /**
   * Get the file size for the local path. For downloads where the local
   * file may not exist yet, returns 0 (which will be below threshold,
   * but the caller should check remote size for downloads of new files).
   */
  private getFileSize(localPath: string, _direction: TransferDirection): number {
    const size = this.getFileSizeFn(localPath);
    // -1 means file doesn't exist — for uploads this means error,
    // for downloads this is expected (new file). In both cases,
    // returning 0 means delta won't be attempted, which is correct
    // for new files that have no base to diff against.
    return size < 0 ? 0 : size;
  }

  /**
   * Race a promise against a timeout. Rejects with a timeout error
   * if the operation exceeds the allowed duration.
   */
  private withTimeout<T>(promise: Promise<T>, timeoutMs: number): Promise<T> {
    return new Promise<T>((resolve, reject) => {
      const timer = setTimeout(() => {
        reject(new Error(`delta transfer timed out after ${timeoutMs}ms`));
      }, timeoutMs);

      promise
        .then((result) => {
          clearTimeout(timer);
          resolve(result);
        })
        .catch((err) => {
          clearTimeout(timer);
          reject(err);
        });
    });
  }
}
