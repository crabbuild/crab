/**
 * Persists transfer records to SQLite for history, statistics, and resume state.
 *
 * Uses better-sqlite3 prepared statements for all queries. Provides paginated
 * history queries with search/sort, aggregate statistics by time period, and
 * resume state management for interrupted transfers.
 */

import type Database from "better-sqlite3";
import type {
  TransferRecord,
  TransferResumeState,
  TransferStats,
} from "./ssh-types";

// ─── Query Options ───────────────────────────────────────────────────────────

export interface TransferHistoryQueryOptions {
  page: number;
  pageSize: number;
  search?: string;
  sortBy?: "timestamp" | "file_size" | "duration" | "average_speed";
  sortDir?: "asc" | "desc";
  profileId?: string;
}

export interface TransferHistoryQueryResult {
  entries: TransferRecord[];
  total: number;
}

// ─── Constants ───────────────────────────────────────────────────────────────

const HISTORY_RETENTION_DAYS = 90;
const MS_PER_DAY = 86_400_000;

// ─── TransferHistoryStore ────────────────────────────────────────────────────

export class TransferHistoryStore {
  private readonly stmtInsert;
  private readonly stmtGetResumeState;
  private readonly stmtUpsertResumeState;
  private readonly stmtPurgeExpiredResume;
  private readonly stmtPurgeOldHistory;

  constructor(private readonly db: Database.Database) {
    this.stmtInsert = this.db.prepare(`
      INSERT INTO transfer_history (
        id, profile_id, direction, local_path, remote_path,
        file_size, compressed_size, duration_ms, average_speed,
        status, error, retry_count, cipher_suite, delta_savings_percent, timestamp
      ) VALUES (
        @id, @profileId, @direction, @localPath, @remotePath,
        @fileSize, @compressedSize, @duration, @averageSpeed,
        @status, @error, @retryCount, @cipherSuite, @deltaSavingsPercent, @timestamp
      )
    `);

    this.stmtGetResumeState = this.db.prepare(`
      SELECT * FROM transfer_resume_state
      WHERE profile_id = ? AND remote_path = ?
      ORDER BY created_at DESC
      LIMIT 1
    `);

    this.stmtUpsertResumeState = this.db.prepare(`
      INSERT OR REPLACE INTO transfer_resume_state (
        id, profile_id, direction, local_path, remote_path,
        bytes_transferred, total_bytes, remote_mtime, remote_size,
        partial_checksum, created_at, expires_at
      ) VALUES (
        @transferId, @profileId, @direction, @localPath, @remotePath,
        @bytesTransferred, @totalBytes, @remoteMtime, @remoteSize,
        @partialChecksum, @createdAt, @expiresAt
      )
    `);

    this.stmtPurgeExpiredResume = this.db.prepare(`
      DELETE FROM transfer_resume_state WHERE expires_at < ?
    `);

    this.stmtPurgeOldHistory = this.db.prepare(`
      DELETE FROM transfer_history WHERE timestamp < ?
    `);
  }

  /** Record a completed or failed transfer. */
  record(entry: TransferRecord): void {
    this.stmtInsert.run({
      id: entry.id,
      profileId: entry.profileId,
      direction: entry.direction,
      localPath: entry.localPath,
      remotePath: entry.remotePath,
      fileSize: entry.fileSize,
      compressedSize: entry.compressedSize ?? null,
      duration: entry.duration,
      averageSpeed: entry.averageSpeed,
      status: entry.status,
      error: entry.error ?? null,
      retryCount: entry.retryCount,
      cipherSuite: entry.cipherSuite,
      deltaSavingsPercent: entry.deltaSavingsPercent ?? null,
      timestamp: entry.timestamp,
    });
  }

  /** Query history with pagination, search, and sort. */
  query(opts: TransferHistoryQueryOptions): TransferHistoryQueryResult {
    const { page, pageSize, search, sortBy, sortDir, profileId } = opts;

    const conditions: string[] = [];
    const params: Record<string, unknown> = {};

    if (profileId) {
      conditions.push("profile_id = @profileId");
      params.profileId = profileId;
    }

    if (search) {
      conditions.push(
        "(local_path LIKE @search OR remote_path LIKE @search)",
      );
      params.search = `%${search}%`;
    }

    const whereClause =
      conditions.length > 0 ? `WHERE ${conditions.join(" AND ")}` : "";

    // Validate sortBy to prevent SQL injection — only allow known columns.
    const allowedSortColumns = new Set([
      "timestamp",
      "file_size",
      "duration",
      "average_speed",
    ]);
    const columnMap: Record<string, string> = {
      timestamp: "timestamp",
      file_size: "file_size",
      duration: "duration_ms",
      average_speed: "average_speed",
    };

    const resolvedSort = sortBy && allowedSortColumns.has(sortBy)
      ? columnMap[sortBy]
      : "timestamp";
    const resolvedDir = sortDir === "asc" ? "ASC" : "DESC";

    const orderClause = `ORDER BY ${resolvedSort} ${resolvedDir}`;

    // Count total matching entries.
    const countSql = `SELECT COUNT(*) as total FROM transfer_history ${whereClause}`;
    const countRow = this.db.prepare(countSql).get(params) as { total: number };
    const total = countRow.total;

    // Fetch the page.
    const offset = (page - 1) * pageSize;
    params.limit = pageSize;
    params.offset = offset;

    const dataSql = `
      SELECT * FROM transfer_history
      ${whereClause}
      ${orderClause}
      LIMIT @limit OFFSET @offset
    `;
    const rows = this.db.prepare(dataSql).all(params) as TransferHistoryRow[];

    const entries = rows.map(rowToTransferRecord);

    return { entries, total };
  }

  /** Get aggregate statistics for a time period. */
  getStats(
    profileId: string,
    period: "day" | "week" | "month",
  ): TransferStats {
    const now = Date.now();
    const periodMs = periodToMs(period);
    const since = now - periodMs;

    const sql = `
      SELECT
        COALESCE(SUM(CASE WHEN direction = 'upload' THEN file_size ELSE 0 END), 0) AS total_uploaded,
        COALESCE(SUM(CASE WHEN direction = 'download' THEN file_size ELSE 0 END), 0) AS total_downloaded,
        COALESCE(AVG(average_speed), 0) AS avg_speed,
        COALESCE(SUM(CASE WHEN status = 'completed' THEN 1 ELSE 0 END), 0) AS success_count,
        COALESCE(SUM(CASE WHEN status = 'failed' THEN 1 ELSE 0 END), 0) AS failure_count
      FROM transfer_history
      WHERE profile_id = @profileId AND timestamp >= @since
    `;

    const row = this.db.prepare(sql).get({ profileId, since }) as StatsRow;

    const totalTransfers = row.success_count + row.failure_count;
    const failureRate =
      totalTransfers > 0 ? (row.failure_count / totalTransfers) * 100 : 0;

    return {
      totalUploaded: row.total_uploaded,
      totalDownloaded: row.total_downloaded,
      averageSpeed: row.avg_speed,
      successCount: row.success_count,
      failureCount: row.failure_count,
      failureRate,
    };
  }

  /** Save resume state for an interrupted transfer. */
  saveResumeState(state: TransferResumeState): void {
    this.stmtUpsertResumeState.run({
      transferId: state.transferId,
      profileId: state.profileId,
      direction: state.direction,
      localPath: state.localPath,
      remotePath: state.remotePath,
      bytesTransferred: state.bytesTransferred,
      totalBytes: state.totalBytes,
      remoteMtime: state.remoteMtime,
      remoteSize: state.remoteSize,
      partialChecksum: state.partialChecksum,
      createdAt: state.createdAt,
      expiresAt: state.expiresAt,
    });
  }

  /** Get resume state for a transfer by profileId and remotePath. */
  getResumeState(
    profileId: string,
    remotePath: string,
  ): TransferResumeState | null {
    const row = this.stmtGetResumeState.get(profileId, remotePath) as
      | ResumeStateRow
      | undefined;

    if (!row) return null;

    return {
      transferId: row.id,
      profileId: row.profile_id,
      direction: row.direction as TransferResumeState["direction"],
      localPath: row.local_path,
      remotePath: row.remote_path,
      bytesTransferred: row.bytes_transferred,
      totalBytes: row.total_bytes,
      remoteMtime: row.remote_mtime,
      remoteSize: row.remote_size,
      partialChecksum: row.partial_checksum,
      createdAt: row.created_at,
      expiresAt: row.expires_at,
    };
  }

  /** Purge expired resume states (>7 days) and old history entries (>90 days). */
  purgeExpired(): void {
    const now = Date.now();

    // Resume states past their expiry.
    this.stmtPurgeExpiredResume.run(now);

    // History entries older than 90 days.
    const historyCutoff = now - HISTORY_RETENTION_DAYS * MS_PER_DAY;
    this.stmtPurgeOldHistory.run(historyCutoff);
  }
}

// ─── Internal Types ──────────────────────────────────────────────────────────

interface TransferHistoryRow {
  id: string;
  profile_id: string;
  direction: string;
  local_path: string;
  remote_path: string;
  file_size: number;
  compressed_size: number | null;
  duration_ms: number;
  average_speed: number;
  status: string;
  error: string | null;
  retry_count: number;
  cipher_suite: string;
  delta_savings_percent: number | null;
  timestamp: number;
}

interface ResumeStateRow {
  id: string;
  profile_id: string;
  direction: string;
  local_path: string;
  remote_path: string;
  bytes_transferred: number;
  total_bytes: number;
  remote_mtime: number;
  remote_size: number;
  partial_checksum: string;
  created_at: number;
  expires_at: number;
}

interface StatsRow {
  total_uploaded: number;
  total_downloaded: number;
  avg_speed: number;
  success_count: number;
  failure_count: number;
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

function rowToTransferRecord(row: TransferHistoryRow): TransferRecord {
  return {
    id: row.id,
    profileId: row.profile_id,
    direction: row.direction as TransferRecord["direction"],
    localPath: row.local_path,
    remotePath: row.remote_path,
    fileSize: row.file_size,
    compressedSize: row.compressed_size ?? undefined,
    duration: row.duration_ms,
    averageSpeed: row.average_speed,
    status: row.status as TransferRecord["status"],
    error: row.error ?? undefined,
    retryCount: row.retry_count,
    cipherSuite: row.cipher_suite,
    deltaSavingsPercent: row.delta_savings_percent ?? undefined,
    timestamp: row.timestamp,
  };
}

function periodToMs(period: "day" | "week" | "month"): number {
  switch (period) {
    case "day":
      return MS_PER_DAY;
    case "week":
      return 7 * MS_PER_DAY;
    case "month":
      return 30 * MS_PER_DAY;
  }
}
