/**
 * Tests for BookmarkManager — SQLite-backed remote path bookmarks
 * and recent file access tracking.
 *
 * Verifies:
 * - addBookmark() validates label length and enforces 50-per-profile limit
 * - removeBookmark() deletes bookmarks by ID
 * - listBookmarks() returns bookmarks ordered by creation time
 * - recordAccess() upserts recent files and enforces 20-per-profile LRU eviction
 * - getRecent() returns recent files ordered by last access descending
 */

import { describe, it, expect, beforeEach, afterEach } from "vitest";
import Database from "better-sqlite3";
import { BookmarkManager } from "../electron/ssh-bookmarks";

// ─── Helpers ─────────────────────────────────────────────────────────────────

function createInMemoryDb(): Database.Database {
  const db = new Database(":memory:");
  db.pragma("journal_mode = WAL");
  db.pragma("foreign_keys = ON");

  db.exec(`
    CREATE TABLE remote_bookmarks (
      id TEXT PRIMARY KEY,
      profile_id TEXT NOT NULL,
      path TEXT NOT NULL,
      label TEXT NOT NULL CHECK (length(label) BETWEEN 1 AND 100),
      created_at INTEGER NOT NULL,
      UNIQUE(profile_id, path)
    );

    CREATE TABLE remote_recent_files (
      profile_id TEXT NOT NULL,
      path TEXT NOT NULL,
      file_name TEXT NOT NULL,
      last_accessed_at INTEGER NOT NULL,
      PRIMARY KEY (profile_id, path)
    );

    CREATE INDEX idx_recent_files_access
      ON remote_recent_files(profile_id, last_accessed_at DESC);
  `);

  return db;
}

// ─── Tests ───────────────────────────────────────────────────────────────────

describe("BookmarkManager", () => {
  let db: Database.Database;
  let manager: BookmarkManager;

  beforeEach(() => {
    db = createInMemoryDb();
    manager = new BookmarkManager(db);
  });

  afterEach(() => {
    db.close();
  });

  describe("addBookmark", () => {
    it("creates a bookmark and returns it with all fields", () => {
      const bookmark = manager.addBookmark("profile-1", "/home/user/project", "My Project");

      expect(bookmark.id).toBeDefined();
      expect(bookmark.profileId).toBe("profile-1");
      expect(bookmark.path).toBe("/home/user/project");
      expect(bookmark.label).toBe("My Project");
      expect(bookmark.createdAt).toBeGreaterThan(0);
    });

    it("persists the bookmark to the database", () => {
      const bookmark = manager.addBookmark("profile-1", "/home/user/src", "Source");

      const row = db.prepare("SELECT * FROM remote_bookmarks WHERE id = ?").get(bookmark.id) as any;
      expect(row).toBeDefined();
      expect(row.profile_id).toBe("profile-1");
      expect(row.path).toBe("/home/user/src");
      expect(row.label).toBe("Source");
    });

    it("rejects labels shorter than 1 character", () => {
      expect(() => manager.addBookmark("profile-1", "/path", "")).toThrow(
        /between 1 and 100 characters/,
      );
    });

    it("rejects labels longer than 100 characters", () => {
      const longLabel = "x".repeat(101);
      expect(() => manager.addBookmark("profile-1", "/path", longLabel)).toThrow(
        /between 1 and 100 characters/,
      );
    });

    it("accepts labels of exactly 1 character", () => {
      const bookmark = manager.addBookmark("profile-1", "/path", "A");
      expect(bookmark.label).toBe("A");
    });

    it("accepts labels of exactly 100 characters", () => {
      const label = "x".repeat(100);
      const bookmark = manager.addBookmark("profile-1", "/path", label);
      expect(bookmark.label).toBe(label);
    });

    it("enforces maximum of 50 bookmarks per profile", () => {
      for (let i = 0; i < 50; i++) {
        manager.addBookmark("profile-1", `/path/${i}`, `Bookmark ${i}`);
      }

      expect(() => manager.addBookmark("profile-1", "/path/51", "One too many")).toThrow(
        /Maximum of 50 bookmarks/,
      );
    });

    it("allows 50 bookmarks per profile independently", () => {
      for (let i = 0; i < 50; i++) {
        manager.addBookmark("profile-1", `/path/${i}`, `Bookmark ${i}`);
      }

      // Different profile should still work.
      const bookmark = manager.addBookmark("profile-2", "/path/0", "First");
      expect(bookmark.profileId).toBe("profile-2");
    });

    it("rejects duplicate path for the same profile", () => {
      manager.addBookmark("profile-1", "/home/user", "First");
      expect(() => manager.addBookmark("profile-1", "/home/user", "Second")).toThrow();
    });

    it("allows same path for different profiles", () => {
      manager.addBookmark("profile-1", "/home/user", "Profile 1");
      const bookmark = manager.addBookmark("profile-2", "/home/user", "Profile 2");
      expect(bookmark.profileId).toBe("profile-2");
    });
  });

  describe("removeBookmark", () => {
    it("removes a bookmark by ID", () => {
      const bookmark = manager.addBookmark("profile-1", "/path/to/remove", "Remove Me");
      manager.removeBookmark(bookmark.id);

      const bookmarks = manager.listBookmarks("profile-1");
      expect(bookmarks).toHaveLength(0);
    });

    it("does not throw when removing a non-existent bookmark", () => {
      expect(() => manager.removeBookmark("non-existent-id")).not.toThrow();
    });

    it("only removes the specified bookmark", () => {
      const b1 = manager.addBookmark("profile-1", "/path/1", "First");
      const b2 = manager.addBookmark("profile-1", "/path/2", "Second");

      manager.removeBookmark(b1.id);

      const bookmarks = manager.listBookmarks("profile-1");
      expect(bookmarks).toHaveLength(1);
      expect(bookmarks[0].id).toBe(b2.id);
    });
  });

  describe("listBookmarks", () => {
    it("returns empty array when no bookmarks exist", () => {
      const bookmarks = manager.listBookmarks("profile-1");
      expect(bookmarks).toEqual([]);
    });

    it("returns bookmarks ordered by creation time ascending", () => {
      manager.addBookmark("profile-1", "/path/a", "Alpha");
      manager.addBookmark("profile-1", "/path/b", "Beta");
      manager.addBookmark("profile-1", "/path/c", "Gamma");

      const bookmarks = manager.listBookmarks("profile-1");
      expect(bookmarks).toHaveLength(3);
      expect(bookmarks[0].label).toBe("Alpha");
      expect(bookmarks[1].label).toBe("Beta");
      expect(bookmarks[2].label).toBe("Gamma");

      // Verify ascending order.
      for (let i = 1; i < bookmarks.length; i++) {
        expect(bookmarks[i].createdAt).toBeGreaterThanOrEqual(bookmarks[i - 1].createdAt);
      }
    });

    it("only returns bookmarks for the specified profile", () => {
      manager.addBookmark("profile-1", "/path/1", "P1");
      manager.addBookmark("profile-2", "/path/2", "P2");

      const bookmarks = manager.listBookmarks("profile-1");
      expect(bookmarks).toHaveLength(1);
      expect(bookmarks[0].label).toBe("P1");
    });
  });

  describe("recordAccess", () => {
    it("records a file access", () => {
      manager.recordAccess("profile-1", "/home/user/file.txt");

      const recent = manager.getRecent("profile-1");
      expect(recent).toHaveLength(1);
      expect(recent[0].path).toBe("/home/user/file.txt");
      expect(recent[0].fileName).toBe("file.txt");
      expect(recent[0].profileId).toBe("profile-1");
      expect(recent[0].lastAccessedAt).toBeGreaterThan(0);
    });

    it("extracts file name from path", () => {
      manager.recordAccess("profile-1", "/deep/nested/dir/report.pdf");

      const recent = manager.getRecent("profile-1");
      expect(recent[0].fileName).toBe("report.pdf");
    });

    it("updates timestamp on re-access of same path", () => {
      manager.recordAccess("profile-1", "/home/user/file.txt");
      const first = manager.getRecent("profile-1")[0].lastAccessedAt;

      // Small delay to ensure different timestamp.
      const laterTime = first + 1000;
      db.prepare(
        "UPDATE remote_recent_files SET last_accessed_at = ? WHERE profile_id = ? AND path = ?",
      ).run(laterTime - 2000, "profile-1", "/home/user/file.txt");

      manager.recordAccess("profile-1", "/home/user/file.txt");
      const second = manager.getRecent("profile-1")[0].lastAccessedAt;

      expect(second).toBeGreaterThan(laterTime - 2000);
    });

    it("enforces maximum of 20 recent files per profile via LRU eviction", () => {
      // Insert 20 files.
      for (let i = 0; i < 20; i++) {
        db.prepare(
          "INSERT INTO remote_recent_files (profile_id, path, file_name, last_accessed_at) VALUES (?, ?, ?, ?)",
        ).run("profile-1", `/path/file-${i}.txt`, `file-${i}.txt`, 1000 + i);
      }

      // Access a 21st file — should evict the oldest (file-0).
      manager.recordAccess("profile-1", "/path/file-new.txt");

      const recent = manager.getRecent("profile-1");
      expect(recent).toHaveLength(20);

      const paths = recent.map((r) => r.path);
      expect(paths).not.toContain("/path/file-0.txt");
      expect(paths).toContain("/path/file-new.txt");
    });

    it("evicts multiple entries when over limit", () => {
      // Insert 22 files directly.
      for (let i = 0; i < 22; i++) {
        db.prepare(
          "INSERT INTO remote_recent_files (profile_id, path, file_name, last_accessed_at) VALUES (?, ?, ?, ?)",
        ).run("profile-1", `/path/file-${i}.txt`, `file-${i}.txt`, 1000 + i);
      }

      // Access another file — should evict 3 oldest to get back to 20.
      manager.recordAccess("profile-1", "/path/file-new.txt");

      const recent = manager.getRecent("profile-1");
      expect(recent).toHaveLength(20);
    });

    it("does not evict entries from other profiles", () => {
      for (let i = 0; i < 20; i++) {
        db.prepare(
          "INSERT INTO remote_recent_files (profile_id, path, file_name, last_accessed_at) VALUES (?, ?, ?, ?)",
        ).run("profile-1", `/path/file-${i}.txt`, `file-${i}.txt`, 1000 + i);
      }

      // Add entries for profile-2.
      db.prepare(
        "INSERT INTO remote_recent_files (profile_id, path, file_name, last_accessed_at) VALUES (?, ?, ?, ?)",
      ).run("profile-2", "/path/other.txt", "other.txt", 5000);

      // Trigger eviction on profile-1.
      manager.recordAccess("profile-1", "/path/file-new.txt");

      // Profile-2 should be unaffected.
      const profile2Recent = manager.getRecent("profile-2");
      expect(profile2Recent).toHaveLength(1);
      expect(profile2Recent[0].path).toBe("/path/other.txt");
    });
  });

  describe("getRecent", () => {
    it("returns empty array when no recent files exist", () => {
      const recent = manager.getRecent("profile-1");
      expect(recent).toEqual([]);
    });

    it("returns recent files ordered by last access descending", () => {
      db.prepare(
        "INSERT INTO remote_recent_files (profile_id, path, file_name, last_accessed_at) VALUES (?, ?, ?, ?)",
      ).run("profile-1", "/path/old.txt", "old.txt", 1000);
      db.prepare(
        "INSERT INTO remote_recent_files (profile_id, path, file_name, last_accessed_at) VALUES (?, ?, ?, ?)",
      ).run("profile-1", "/path/mid.txt", "mid.txt", 2000);
      db.prepare(
        "INSERT INTO remote_recent_files (profile_id, path, file_name, last_accessed_at) VALUES (?, ?, ?, ?)",
      ).run("profile-1", "/path/new.txt", "new.txt", 3000);

      const recent = manager.getRecent("profile-1");
      expect(recent).toHaveLength(3);
      expect(recent[0].fileName).toBe("new.txt");
      expect(recent[1].fileName).toBe("mid.txt");
      expect(recent[2].fileName).toBe("old.txt");
    });

    it("only returns recent files for the specified profile", () => {
      db.prepare(
        "INSERT INTO remote_recent_files (profile_id, path, file_name, last_accessed_at) VALUES (?, ?, ?, ?)",
      ).run("profile-1", "/path/p1.txt", "p1.txt", 1000);
      db.prepare(
        "INSERT INTO remote_recent_files (profile_id, path, file_name, last_accessed_at) VALUES (?, ?, ?, ?)",
      ).run("profile-2", "/path/p2.txt", "p2.txt", 2000);

      const recent = manager.getRecent("profile-1");
      expect(recent).toHaveLength(1);
      expect(recent[0].fileName).toBe("p1.txt");
    });
  });
});
