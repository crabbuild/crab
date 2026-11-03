/**
 * File Transfer Manager — SFTP-based upload/download with progress
 * tracking, transfer queue, and retry logic.
 *
 * Supports:
 * - Single file upload/download with progress events
 * - Folder upload/download (recursive)
 * - Transfer queue with configurable concurrency
 * - Drag-and-drop upload integration
 * - Retry on transient errors
 * - Cancellation
 */

import { EventEmitter } from "node:events";
import * as fs from "node:fs";
import * as path from "node:path";
import type { SFTPWrapper } from "ssh2";
import type { SshSessionManager } from "./ssh-manager";

// ── Types ────────────────────────────────────────────────────────

export type TransferDirection = "upload" | "download";
export type TransferStatus = "queued" | "active" | "completed" | "failed" | "cancelled";

export interface Transfer {
  id: string;
  profileId: string;
  direction: TransferDirection;
  localPath: string;
  remotePath: string;
  status: TransferStatus;
  bytesTransferred: number;
  totalBytes: number;
  speed: number;
  eta: number;
  startedAt?: number;
  completedAt?: number;
  error?: string;
  retryCount: number;
}

export interface TransferProgress {
  transferId: string;
  percent: number;
  speed: number;
  eta: number;
  bytesTransferred: number;
  totalBytes: number;
}

export interface FileTransferManagerEvents {
  "transfer:progress": (progress: TransferProgress) => void;
  "transfer:completed": (transfer: Transfer) => void;
  "transfer:failed": (transfer: Transfer) => void;
  "transfer:cancelled": (transfer: Transfer) => void;
  "transfer:queued": (transfer: Transfer) => void;
}

export interface FileTransferOptions {
  /** Maximum concurrent transfers (default: 2). */
  maxConcurrent?: number;
  /** Maximum retry attempts for transient errors (default: 3). */
  maxRetries?: number;
  /** Progress update interval in ms (default: 250). */
  progressIntervalMs?: number;
}

// ── Constants ────────────────────────────────────────────────────

const DEFAULT_MAX_CONCURRENT = 2;
const DEFAULT_MAX_RETRIES = 3;
const DEFAULT_PROGRESS_INTERVAL_MS = 250;

// ── File Transfer Manager ────────────────────────────────────────

export class FileTransferManager extends EventEmitter {
  private sessionManager: SshSessionManager;
  private transfers = new Map<string, Transfer>();
  private queue: string[] = [];
  private activeCount = 0;
  private maxConcurrent: number;
  private maxRetries: number;
  private progressIntervalMs: number;
  private cancelledIds = new Set<string>();

  constructor(sessionManager: SshSessionManager, options?: FileTransferOptions) {
    super();
    this.sessionManager = sessionManager;
    this.maxConcurrent = options?.maxConcurrent ?? DEFAULT_MAX_CONCURRENT;
    this.maxRetries = options?.maxRetries ?? DEFAULT_MAX_RETRIES;
    this.progressIntervalMs = options?.progressIntervalMs ?? DEFAULT_PROGRESS_INTERVAL_MS;
  }

  /**
   * Download a file from the remote server to a local path.
   * Returns the transfer ID for tracking.
   */
  download(profileId: string, remotePath: string, localPath: string): string {
    const id = crypto.randomUUID();
    const transfer: Transfer = {
      id,
      profileId,
      direction: "download",
      localPath,
      remotePath,
      status: "queued",
      bytesTransferred: 0,
      totalBytes: 0,
      speed: 0,
      eta: 0,
      retryCount: 0,
    };

    this.transfers.set(id, transfer);
    this.queue.push(id);
    this.emit("transfer:queued", transfer);
    this.processQueue();

    return id;
  }

  /**
   * Upload a file from the local filesystem to the remote server.
   * Returns the transfer ID for tracking.
   */
  upload(profileId: string, localPath: string, remotePath: string): string {
    const id = crypto.randomUUID();
    const transfer: Transfer = {
      id,
      profileId,
      direction: "upload",
      localPath,
      remotePath,
      status: "queued",
      bytesTransferred: 0,
      totalBytes: 0,
      speed: 0,
      eta: 0,
      retryCount: 0,
    };

    this.transfers.set(id, transfer);
    this.queue.push(id);
    this.emit("transfer:queued", transfer);
    this.processQueue();

    return id;
  }

  /**
   * Cancel an active or queued transfer.
   */
  cancel(transferId: string): boolean {
    const transfer = this.transfers.get(transferId);
    if (!transfer) return false;

    if (transfer.status === "queued") {
      this.queue = this.queue.filter((id) => id !== transferId);
      transfer.status = "cancelled";
      this.emit("transfer:cancelled", transfer);
      return true;
    }

    if (transfer.status === "active") {
      this.cancelledIds.add(transferId);
      return true;
    }

    return false;
  }

  /**
   * Get the current status of a transfer.
   */
  getTransfer(transferId: string): Transfer | undefined {
    return this.transfers.get(transferId);
  }

  /**
   * List all transfers (optionally filtered by status).
   */
  listTransfers(status?: TransferStatus): Transfer[] {
    const all = [...this.transfers.values()];
    return status ? all.filter((t) => t.status === status) : all;
  }

  /**
   * Upload multiple files (e.g., from drag-and-drop).
   * Returns an array of transfer IDs.
   */
  uploadMultiple(
    profileId: string,
    files: Array<{ localPath: string; remotePath: string }>,
  ): string[] {
    return files.map((f) => this.upload(profileId, f.localPath, f.remotePath));
  }

  /**
   * Download an entire directory recursively.
   */
  async downloadFolder(
    profileId: string,
    remotePath: string,
    localPath: string,
  ): Promise<string[]> {
    const sftp = this.getSftp(profileId);
    if (!sftp) throw new Error("SFTP channel not available");

    const transferIds: string[] = [];
    await this.downloadFolderRecursive(sftp, profileId, remotePath, localPath, transferIds);
    return transferIds;
  }

  /**
   * Upload an entire directory recursively.
   */
  async uploadFolder(
    profileId: string,
    localPath: string,
    remotePath: string,
  ): Promise<string[]> {
    const transferIds: string[] = [];
    await this.uploadFolderRecursive(profileId, localPath, remotePath, transferIds);
    return transferIds;
  }

  // ── Private ────────────────────────────────────────────────────

  private processQueue(): void {
    while (this.activeCount < this.maxConcurrent && this.queue.length > 0) {
      const transferId = this.queue.shift();
      if (!transferId) break;

      const transfer = this.transfers.get(transferId);
      if (!transfer || transfer.status !== "queued") continue;

      this.activeCount++;
      this.executeTransfer(transfer).finally(() => {
        this.activeCount--;
        this.processQueue();
      });
    }
  }

  private async executeTransfer(transfer: Transfer): Promise<void> {
    transfer.status = "active";
    transfer.startedAt = Date.now();

    try {
      if (transfer.direction === "download") {
        await this.executeDownload(transfer);
      } else {
        await this.executeUpload(transfer);
      }

      if (this.cancelledIds.has(transfer.id)) {
        this.cancelledIds.delete(transfer.id);
        transfer.status = "cancelled";
        this.emit("transfer:cancelled", transfer);
        return;
      }

      transfer.status = "completed";
      transfer.completedAt = Date.now();
      this.emit("transfer:completed", transfer);
    } catch (err) {
      if (this.cancelledIds.has(transfer.id)) {
        this.cancelledIds.delete(transfer.id);
        transfer.status = "cancelled";
        this.emit("transfer:cancelled", transfer);
        return;
      }

      // Retry on transient errors
      if (transfer.retryCount < this.maxRetries && this.isTransientError(err)) {
        transfer.retryCount++;
        transfer.status = "queued";
        this.queue.push(transfer.id);
        return;
      }

      transfer.status = "failed";
      transfer.error = err instanceof Error ? err.message : String(err);
      transfer.completedAt = Date.now();
      this.emit("transfer:failed", transfer);
    }
  }

  private async executeDownload(transfer: Transfer): Promise<void> {
    const sftp = this.getSftp(transfer.profileId);
    if (!sftp) throw new Error("SFTP channel not available");

    // Get remote file size
    const stats = await new Promise<{ size: number }>((resolve, reject) => {
      sftp.stat(transfer.remotePath, (err, stats) => {
        if (err) reject(err);
        else resolve(stats as unknown as { size: number });
      });
    });

    transfer.totalBytes = stats.size;

    // Ensure local directory exists
    const dir = path.dirname(transfer.localPath);
    fs.mkdirSync(dir, { recursive: true });

    // Stream download
    await new Promise<void>((resolve, reject) => {
      const readStream = sftp.createReadStream(transfer.remotePath);
      const writeStream = fs.createWriteStream(transfer.localPath);
      let lastProgressAt = 0;

      readStream.on("data", (chunk: Buffer | string) => {
        if (this.cancelledIds.has(transfer.id)) {
          readStream.destroy();
          writeStream.destroy();
          return;
        }

        const len = typeof chunk === "string" ? Buffer.byteLength(chunk) : chunk.length;
        transfer.bytesTransferred += len;
        const now = Date.now();

        if (now - lastProgressAt >= this.progressIntervalMs) {
          lastProgressAt = now;
          this.emitProgress(transfer);
        }
      });

      readStream.on("error", reject);
      writeStream.on("error", reject);
      writeStream.on("finish", resolve);

      readStream.pipe(writeStream);
    });
  }

  private async executeUpload(transfer: Transfer): Promise<void> {
    const sftp = this.getSftp(transfer.profileId);
    if (!sftp) throw new Error("SFTP channel not available");

    // Get local file size
    const stats = fs.statSync(transfer.localPath);
    transfer.totalBytes = stats.size;

    // Stream upload
    await new Promise<void>((resolve, reject) => {
      const readStream = fs.createReadStream(transfer.localPath);
      const writeStream = sftp.createWriteStream(transfer.remotePath);
      let lastProgressAt = 0;

      readStream.on("data", (chunk: Buffer | string) => {
        if (this.cancelledIds.has(transfer.id)) {
          readStream.destroy();
          writeStream.destroy();
          return;
        }

        const len = typeof chunk === "string" ? Buffer.byteLength(chunk) : chunk.length;
        transfer.bytesTransferred += len;
        const now = Date.now();

        if (now - lastProgressAt >= this.progressIntervalMs) {
          lastProgressAt = now;
          this.emitProgress(transfer);
        }
      });

      readStream.on("error", reject);
      writeStream.on("error", reject);
      writeStream.on("finish", resolve);

      readStream.pipe(writeStream);
    });
  }

  private emitProgress(transfer: Transfer): void {
    const elapsed = Date.now() - (transfer.startedAt ?? Date.now());
    const speed = elapsed > 0 ? (transfer.bytesTransferred / elapsed) * 1000 : 0;
    const remaining = transfer.totalBytes - transfer.bytesTransferred;
    const eta = speed > 0 ? remaining / speed : 0;

    transfer.speed = speed;
    transfer.eta = eta;

    const percent = transfer.totalBytes > 0
      ? Math.round((transfer.bytesTransferred / transfer.totalBytes) * 100)
      : 0;

    this.emit("transfer:progress", {
      transferId: transfer.id,
      percent,
      speed,
      eta,
      bytesTransferred: transfer.bytesTransferred,
      totalBytes: transfer.totalBytes,
    });
  }

  private async downloadFolderRecursive(
    sftp: SFTPWrapper,
    profileId: string,
    remotePath: string,
    localPath: string,
    transferIds: string[],
  ): Promise<void> {
    fs.mkdirSync(localPath, { recursive: true });

    const entries = await new Promise<Array<{ filename: string; longname: string; attrs: { isDirectory(): boolean } }>>((resolve, reject) => {
      sftp.readdir(remotePath, (err, list) => {
        if (err) reject(err);
        else resolve(list as any);
      });
    });

    for (const entry of entries) {
      const remoteEntryPath = `${remotePath}/${entry.filename}`;
      const localEntryPath = path.join(localPath, entry.filename);

      if (entry.attrs.isDirectory()) {
        await this.downloadFolderRecursive(sftp, profileId, remoteEntryPath, localEntryPath, transferIds);
      } else {
        const id = this.download(profileId, remoteEntryPath, localEntryPath);
        transferIds.push(id);
      }
    }
  }

  private async uploadFolderRecursive(
    profileId: string,
    localPath: string,
    remotePath: string,
    transferIds: string[],
  ): Promise<void> {
    const entries = fs.readdirSync(localPath, { withFileTypes: true });

    for (const entry of entries) {
      const localEntryPath = path.join(localPath, entry.name);
      const remoteEntryPath = `${remotePath}/${entry.name}`;

      if (entry.isDirectory()) {
        await this.uploadFolderRecursive(profileId, localEntryPath, remoteEntryPath, transferIds);
      } else {
        const id = this.upload(profileId, localEntryPath, remoteEntryPath);
        transferIds.push(id);
      }
    }
  }

  private getSftp(profileId: string): SFTPWrapper | undefined {
    const conn = this.sessionManager.getConnection(profileId);
    return conn?.channels.sftp;
  }

  private isTransientError(err: unknown): boolean {
    if (err instanceof Error) {
      const msg = err.message.toLowerCase();
      return (
        msg.includes("timeout") ||
        msg.includes("econnreset") ||
        msg.includes("epipe") ||
        msg.includes("channel closed")
      );
    }
    return false;
  }
}
