/**
 * SQLite database for SSH enhanced features persistence.
 *
 * Stores transfer history, resume state, scheduled transfers, bookmarks,
 * recent files, workspace sync pairs, and sync conflicts.
 * Uses better-sqlite3 for synchronous, fast access from the Electron main process.
 *
 * Database location: ~/.crab/desktop/ssh-enhanced.db
 */

import Database from "better-sqlite3";
import * as fs from "node:fs";
import * as path from "node:path";

// ─── Constants ───────────────────────────────────────────────────────────────


// ─── SshDatabase ─────────────────────────────────────────────────────────────

/**
 * Manages the SQLite database for SSH enhanced features. Handles schema
 * creation and migrations. Provides the raw database handle for use by
 * feature-specific store classes (TransferHistoryStore, BookmarkManager, etc.).
 */
export class SshDatabase {
  private db: Database.Database;

  constructor(resolvedPath: string) {

    // Ensure directory exists.
    fs.mkdirSync(path.dirname(resolvedPath), { recursive: true });

    this.db = new Database(resolvedPath);

    // Enable WAL mode for better concurrent read/write performance.
    this.db.pragma("journal_mode = WAL");
    this.db.pragma("synchronous = NORMAL");
    this.db.pragma("foreign_keys = ON");

    this.migrate();
  }

  /** Returns the underlying better-sqlite3 Database instance. */
  getDb(): Database.Database {
    return this.db;
  }

  /** Closes the database connection. Call on app quit. */
  close(): void {
    this.db.close();
  }

  // ─── Schema Migration ────────────────────────────────────────────────────

  private migrate(): void {
    this.db.exec(`
      -- Transfer history
      CREATE TABLE IF NOT EXISTS transfer_history (
        id TEXT PRIMARY KEY,
        profile_id TEXT NOT NULL,
        direction TEXT NOT NULL CHECK (direction IN ('upload', 'download')),
        local_path TEXT NOT NULL,
        remote_path TEXT NOT NULL,
        file_size INTEGER NOT NULL,
        compressed_size INTEGER,
        duration_ms INTEGER NOT NULL,
        average_speed REAL NOT NULL,
        status TEXT NOT NULL CHECK (status IN ('completed', 'failed')),
        error TEXT,
        retry_count INTEGER NOT NULL DEFAULT 0,
        cipher_suite TEXT NOT NULL,
        mac_algorithm TEXT,
        kex_method TEXT,
        delta_savings_percent REAL,
        timestamp INTEGER NOT NULL
      );

      CREATE INDEX IF NOT EXISTS idx_transfer_history_profile_ts
        ON transfer_history(profile_id, timestamp DESC);

      CREATE INDEX IF NOT EXISTS idx_transfer_history_search
        ON transfer_history(local_path, remote_path);

      -- Transfer resume state
      CREATE TABLE IF NOT EXISTS transfer_resume_state (
        id TEXT PRIMARY KEY,
        profile_id TEXT NOT NULL,
        direction TEXT NOT NULL CHECK (direction IN ('upload', 'download')),
        local_path TEXT NOT NULL,
        remote_path TEXT NOT NULL,
        bytes_transferred INTEGER NOT NULL,
        total_bytes INTEGER NOT NULL,
        remote_mtime INTEGER NOT NULL,
        remote_size INTEGER NOT NULL,
        partial_checksum TEXT NOT NULL,
        created_at INTEGER NOT NULL,
        expires_at INTEGER NOT NULL
      );

      CREATE INDEX IF NOT EXISTS idx_resume_state_lookup
        ON transfer_resume_state(profile_id, remote_path);

      -- Scheduled transfers
      CREATE TABLE IF NOT EXISTS scheduled_transfers (
        id TEXT PRIMARY KEY,
        profile_id TEXT NOT NULL,
        direction TEXT NOT NULL CHECK (direction IN ('upload', 'download')),
        local_path TEXT NOT NULL,
        remote_path TEXT NOT NULL,
        scheduled_at INTEGER NOT NULL,
        created_at INTEGER NOT NULL,
        status TEXT NOT NULL DEFAULT 'pending'
          CHECK (status IN ('pending', 'executing', 'completed', 'failed', 'cancelled')),
        error TEXT
      );

      CREATE INDEX IF NOT EXISTS idx_scheduled_profile
        ON scheduled_transfers(profile_id, status);

      -- Remote path bookmarks
      CREATE TABLE IF NOT EXISTS remote_bookmarks (
        id TEXT PRIMARY KEY,
        profile_id TEXT NOT NULL,
        path TEXT NOT NULL,
        label TEXT NOT NULL CHECK (length(label) BETWEEN 1 AND 100),
        created_at INTEGER NOT NULL,
        UNIQUE(profile_id, path)
      );

      -- Recent remote files
      CREATE TABLE IF NOT EXISTS remote_recent_files (
        profile_id TEXT NOT NULL,
        path TEXT NOT NULL,
        file_name TEXT NOT NULL,
        last_accessed_at INTEGER NOT NULL,
        PRIMARY KEY (profile_id, path)
      );

      CREATE INDEX IF NOT EXISTS idx_recent_files_access
        ON remote_recent_files(profile_id, last_accessed_at DESC);

      -- Workspace sync pairs
      CREATE TABLE IF NOT EXISTS workspace_sync_pairs (
        id TEXT PRIMARY KEY,
        profile_id TEXT NOT NULL,
        local_dir TEXT NOT NULL,
        remote_dir TEXT NOT NULL,
        ignore_patterns TEXT NOT NULL DEFAULT '[]',
        status TEXT NOT NULL DEFAULT 'paused'
          CHECK (status IN ('active', 'paused', 'error')),
        last_sync_at INTEGER,
        created_at INTEGER NOT NULL
      );

      -- Sync conflicts
      CREATE TABLE IF NOT EXISTS sync_conflicts (
        id TEXT PRIMARY KEY,
        sync_pair_id TEXT NOT NULL,
        file_path TEXT NOT NULL,
        local_mtime INTEGER NOT NULL,
        remote_mtime INTEGER NOT NULL,
        detected_at INTEGER NOT NULL,
        resolved_at INTEGER,
        resolution TEXT CHECK (resolution IN ('local', 'remote', 'both')),
        FOREIGN KEY (sync_pair_id) REFERENCES workspace_sync_pairs(id) ON DELETE CASCADE
      );

      -- Remote workspace sources (multi-repo discovery targets)
      CREATE TABLE IF NOT EXISTS remote_workspace_sources (
        id TEXT PRIMARY KEY,
        workspace_root TEXT NOT NULL,
        profile_id TEXT NOT NULL,
        profile_label TEXT NOT NULL,
        root_path TEXT NOT NULL,
        max_depth INTEGER DEFAULT 3,
        exclude_patterns TEXT DEFAULT '[]',
        created_at INTEGER NOT NULL,
        UNIQUE(workspace_root, profile_id, root_path)
      );
    `);
  }
}
