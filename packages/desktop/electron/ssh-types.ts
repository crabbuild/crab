/**
 * Shared TypeScript interfaces and types for the enhanced SSH subsystem.
 *
 * These types are used across the main process modules and the renderer
 * for file transfers, workspace sync, process management, connection
 * quality monitoring, bookmarks, and agent capability negotiation.
 */

// ── Transfer Types ───────────────────────────────────────────────

export type TransferDirection = "upload" | "download";

export type TransferStatus =
  | "scheduled"
  | "queued"
  | "resuming"
  | "active"
  | "completed"
  | "failed"
  | "cancelled";

export interface TransferEntry {
  id: string;
  profileId: string;
  direction: TransferDirection;
  localPath: string;
  remotePath: string;
  fileName: string;
  status: TransferStatus;
  bytesTransferred: number;
  totalBytes: number;
  compressedBytes?: number;
  speed: number;
  eta: number;
  startedAt?: number;
  completedAt?: number;
  error?: string;
  retryCount: number;
  deltaSavingsPercent?: number;
  scheduledAt?: number;
  cipherSuite?: string;
}

export interface TransferRecord {
  id: string;
  profileId: string;
  direction: TransferDirection;
  localPath: string;
  remotePath: string;
  fileSize: number;
  compressedSize?: number;
  duration: number;
  averageSpeed: number;
  status: "completed" | "failed";
  error?: string;
  retryCount: number;
  cipherSuite: string;
  timestamp: number;
  deltaSavingsPercent?: number;
}


export interface TransferResumeState {
  transferId: string;
  profileId: string;
  direction: TransferDirection;
  localPath: string;
  remotePath: string;
  bytesTransferred: number;
  totalBytes: number;
  remoteMtime: number;
  remoteSize: number;
  partialChecksum: string;
  createdAt: number;
  expiresAt: number;
}

export interface TransferStats {
  totalUploaded: number;
  totalDownloaded: number;
  averageSpeed: number;
  successCount: number;
  failureCount: number;
  failureRate: number;
}

// ── Scheduling Types ─────────────────────────────────────────────

export interface ScheduledTransfer {
  id: string;
  profileId: string;
  direction: TransferDirection;
  localPath: string;
  remotePath: string;
  scheduledAt: number;
  createdAt: number;
}

// ── Workspace Sync Types ─────────────────────────────────────────

export interface SyncPair {
  id: string;
  profileId: string;
  localDir: string;
  remoteDir: string;
  ignorePatterns: string[];
  status: "active" | "paused" | "error";
  lastSyncAt?: number;
  pendingChanges: number;
}

export interface SyncConflict {
  id: string;
  syncPairId: string;
  filePath: string;
  localMtime: number;
  remoteMtime: number;
  detectedAt: number;
  resolvedAt?: number;
  resolution?: "local" | "remote" | "both";
}

// ── Process Management Types ─────────────────────────────────────

export interface RemoteProcess {
  pid: number;
  name: string;
  cpuPercent: number;
  memoryPercent: number;
  memoryBytes: number;
  user: string;
  runtime: number;
}

// ── Connection Quality Types ─────────────────────────────────────

export type QualityLevel = "green" | "yellow" | "red";

export interface ConnectionQuality {
  level: QualityLevel;
  latencyMs: number;
  bandwidthMbps: number;
  packetLossPercent: number;
  measuredAt: number;
}

// ── Agent Capabilities Types ─────────────────────────────────────

export interface AgentCapabilities {
  protocol: 1;
  version: string;
  methods: string[];
}

// ── Bookmark and Recent File Types ───────────────────────────────

export interface RemoteBookmark {
  id: string;
  profileId: string;
  path: string;
  label: string;
  createdAt: number;
}

export interface RecentFile {
  profileId: string;
  path: string;
  fileName: string;
  lastAccessedAt: number;
}

// ── Error Types ──────────────────────────────────────────────────

export interface SshIpcError {
  code: string;
  message: string;
  suggestion?: string;
  retryable: boolean;
}
