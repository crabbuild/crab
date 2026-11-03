/**
 * Manages remote path bookmarks and recent file access for SSH profiles.
 *
 * Persists bookmarks and recent files to SQLite. Enforces per-profile limits:
 * 50 bookmarks and 20 recent files with LRU eviction. Uses better-sqlite3
 * prepared statements for all queries.
 */

import type Database from "better-sqlite3";
import { randomUUID } from "node:crypto";
import type { RecentFile, RemoteBookmark } from "./ssh-types";

// ─── Constants ───────────────────────────────────────────────────────────────

const MAX_BOOKMARKS_PER_PROFILE = 50;
const MAX_RECENT_FILES_PER_PROFILE = 20;
const LABEL_MIN_LENGTH = 1;
const LABEL_MAX_LENGTH = 100;

// ─── BookmarkManager ─────────────────────────────────────────────────────────

export class BookmarkManager {
  private readonly stmtInsertBookmark;
  private readonly stmtDeleteBookmark;
  private readonly stmtListBookmarks;
  private readonly stmtCountBookmarks;
  private readonly stmtUpsertRecent;
  private readonly stmtListRecent;
  private readonly stmtCountRecent;
  private readonly stmtDeleteOldestRecent;

  constructor(private readonly db: Database.Database) {
    this.stmtInsertBookmark = this.db.prepare(`
      INSERT INTO remote_bookmarks (id, profile_id, path, label, created_at)
      VALUES (@id, @profileId, @path, @label, @createdAt)
    `);

    this.stmtDeleteBookmark = this.db.prepare(`
      DELETE FROM remote_bookmarks WHERE id = ?
    `);

    this.stmtListBookmarks = this.db.prepare(`
      SELECT id, profile_id, path, label, created_at
      FROM remote_bookmarks
      WHERE profile_id = ?
      ORDER BY created_at ASC
    `);

    this.stmtCountBookmarks = this.db.prepare(`
      SELECT COUNT(*) AS count FROM remote_bookmarks WHERE profile_id = ?
    `);

    this.stmtUpsertRecent = this.db.prepare(`
      INSERT INTO remote_recent_files (profile_id, path, file_name, last_accessed_at)
      VALUES (@profileId, @path, @fileName, @lastAccessedAt)
      ON CONFLICT (profile_id, path)
      DO UPDATE SET last_accessed_at = @lastAccessedAt, file_name = @fileName
    `);

    this.stmtListRecent = this.db.prepare(`
      SELECT profile_id, path, file_name, last_accessed_at
      FROM remote_recent_files
      WHERE profile_id = ?
      ORDER BY last_accessed_at DESC
    `);

    this.stmtCountRecent = this.db.prepare(`
      SELECT COUNT(*) AS count FROM remote_recent_files WHERE profile_id = ?
    `);

    this.stmtDeleteOldestRecent = this.db.prepare(`
      DELETE FROM remote_recent_files
      WHERE profile_id = @profileId
        AND path IN (
          SELECT path FROM remote_recent_files
          WHERE profile_id = @profileId
          ORDER BY last_accessed_at ASC
          LIMIT @excess
        )
    `);
  }

  /** Add a bookmark for a remote path. Validates label and enforces max 50 per profile. */
  addBookmark(profileId: string, path: string, label: string): RemoteBookmark {
    if (label.length < LABEL_MIN_LENGTH || label.length > LABEL_MAX_LENGTH) {
      throw new Error(
        `Bookmark label must be between ${LABEL_MIN_LENGTH} and ${LABEL_MAX_LENGTH} characters`,
      );
    }

    const count = (
      this.stmtCountBookmarks.get(profileId) as { count: number }
    ).count;

    if (count >= MAX_BOOKMARKS_PER_PROFILE) {
      throw new Error(
        `Maximum of ${MAX_BOOKMARKS_PER_PROFILE} bookmarks per profile reached`,
      );
    }

    const bookmark: RemoteBookmark = {
      id: randomUUID(),
      profileId,
      path,
      label,
      createdAt: Date.now(),
    };

    this.stmtInsertBookmark.run({
      id: bookmark.id,
      profileId: bookmark.profileId,
      path: bookmark.path,
      label: bookmark.label,
      createdAt: bookmark.createdAt,
    });

    return bookmark;
  }

  /** Remove a bookmark by its ID. */
  removeBookmark(bookmarkId: string): void {
    this.stmtDeleteBookmark.run(bookmarkId);
  }

  /** List all bookmarks for a profile, ordered by creation time ascending. */
  listBookmarks(profileId: string): RemoteBookmark[] {
    const rows = this.stmtListBookmarks.all(profileId) as BookmarkRow[];
    return rows.map(rowToBookmark);
  }

  /** Record a file access. Upserts into recent files and enforces max 20 per profile via LRU eviction. */
  recordAccess(profileId: string, path: string): void {
    const fileName = path.split("/").pop() || path;

    this.stmtUpsertRecent.run({
      profileId,
      path,
      fileName,
      lastAccessedAt: Date.now(),
    });

    // Evict oldest entries if over the limit.
    const count = (
      this.stmtCountRecent.get(profileId) as { count: number }
    ).count;

    if (count > MAX_RECENT_FILES_PER_PROFILE) {
      const excess = count - MAX_RECENT_FILES_PER_PROFILE;
      this.stmtDeleteOldestRecent.run({ profileId, excess });
    }
  }

  /** Get recent files for a profile, ordered by last access time descending. */
  getRecent(profileId: string): RecentFile[] {
    const rows = this.stmtListRecent.all(profileId) as RecentFileRow[];
    return rows.map(rowToRecentFile);
  }
}

// ─── Internal Types ──────────────────────────────────────────────────────────

interface BookmarkRow {
  id: string;
  profile_id: string;
  path: string;
  label: string;
  created_at: number;
}

interface RecentFileRow {
  profile_id: string;
  path: string;
  file_name: string;
  last_accessed_at: number;
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

function rowToBookmark(row: BookmarkRow): RemoteBookmark {
  return {
    id: row.id,
    profileId: row.profile_id,
    path: row.path,
    label: row.label,
    createdAt: row.created_at,
  };
}

function rowToRecentFile(row: RecentFileRow): RecentFile {
  return {
    profileId: row.profile_id,
    path: row.path,
    fileName: row.file_name,
    lastAccessedAt: row.last_accessed_at,
  };
}
