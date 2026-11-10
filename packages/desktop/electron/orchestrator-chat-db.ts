/**
 * SQLite-backed Chat Persistence for the Crab Desktop Orchestrator.
 *
 * Stores chat messages in a SQLite database with full-text search support.
 * Uses better-sqlite3 for synchronous, fast access from the Electron main process.
 *
 * Database location: ~/.crab/desktop/agent-chats.db
 */

import Database from "better-sqlite3";
import * as fs from "node:fs";
import * as path from "node:path";
import * as os from "node:os";

// ─── Types ───────────────────────────────────────────────────────────────────

export interface ChatMessageRow {
  id: string;
  task_id: string;
  role: "user" | "agent";
  kind: string | null;
  content: string;
  events: string | null; // JSON-serialized ToolEvent[]
  timestamp: number;
  metadata?: string | null;
}

export interface SearchResult {
  id: string;
  task_id: string;
  role: string;
  content: string;
  timestamp: number;
  snippet: string;
}

// ─── Constants ───────────────────────────────────────────────────────────────

const DB_DIR = path.join(os.homedir(), ".crab", "desktop");
const DB_PATH = path.join(DB_DIR, "agent-chats.db");

// ─── ChatDatabase ────────────────────────────────────────────────────────────

export class ChatDatabase {
  private db: Database.Database;

  constructor(dbPath?: string) {
    const resolvedPath = dbPath ?? DB_PATH;

    // Ensure directory exists.
    fs.mkdirSync(path.dirname(resolvedPath), { recursive: true });

    this.db = new Database(resolvedPath);

    // Enable WAL mode for better concurrent read/write performance.
    this.db.pragma("journal_mode = WAL");
    this.db.pragma("synchronous = NORMAL");

    this.migrate();
  }

  // ─── Schema Migration ────────────────────────────────────────────────────

  private migrate(): void {
    this.db.exec(`
      CREATE TABLE IF NOT EXISTS messages (
        id TEXT PRIMARY KEY,
        task_id TEXT NOT NULL,
        role TEXT NOT NULL,
        kind TEXT,
        content TEXT NOT NULL DEFAULT '',
        events TEXT,
        timestamp INTEGER NOT NULL
      );

      CREATE INDEX IF NOT EXISTS idx_messages_task
        ON messages(task_id, timestamp);

      CREATE INDEX IF NOT EXISTS idx_messages_timestamp
        ON messages(timestamp);
    `);
    const columns = this.db.prepare("PRAGMA table_info(messages)").all() as Array<{ name: string }>;
    if (!columns.some(column => column.name === "metadata")) this.db.exec("ALTER TABLE messages ADD COLUMN metadata TEXT");

    // FTS5 virtual table for full-text search.
    // Check if it already exists to avoid errors on re-migration.
    const ftsExists = this.db
      .prepare("SELECT name FROM sqlite_master WHERE type='table' AND name='messages_fts'")
      .get();

    if (!ftsExists) {
      this.db.exec(`
        CREATE VIRTUAL TABLE messages_fts USING fts5(
          content,
          task_id UNINDEXED,
          content='messages',
          content_rowid='rowid'
        );

        -- Triggers to keep FTS index in sync.
        CREATE TRIGGER IF NOT EXISTS messages_ai AFTER INSERT ON messages BEGIN
          INSERT INTO messages_fts(rowid, content, task_id)
            VALUES (new.rowid, new.content, new.task_id);
        END;

        CREATE TRIGGER IF NOT EXISTS messages_ad AFTER DELETE ON messages BEGIN
          INSERT INTO messages_fts(messages_fts, rowid, content, task_id)
            VALUES ('delete', old.rowid, old.content, old.task_id);
        END;

        CREATE TRIGGER IF NOT EXISTS messages_au AFTER UPDATE ON messages BEGIN
          INSERT INTO messages_fts(messages_fts, rowid, content, task_id)
            VALUES ('delete', old.rowid, old.content, old.task_id);
          INSERT INTO messages_fts(rowid, content, task_id)
            VALUES (new.rowid, new.content, new.task_id);
        END;
      `);
    }
  }

  // ─── Write Operations ────────────────────────────────────────────────────

  /**
   * Inserts a single message. If a message with the same ID already exists,
   * it's replaced (upsert).
   */
  insertMessage(msg: ChatMessageRow): void {
    const stmt = this.db.prepare(`
      INSERT INTO messages (id, task_id, role, kind, content, events, timestamp, metadata)
      VALUES (?, ?, ?, ?, ?, ?, ?, ?)
      ON CONFLICT(id) DO UPDATE SET kind=excluded.kind, content=excluded.content,
        events=excluded.events, timestamp=excluded.timestamp, metadata=excluded.metadata
    `);
    stmt.run(msg.id, msg.task_id, msg.role, msg.kind, msg.content, msg.events, msg.timestamp, msg.metadata ?? null);
  }

  /**
   * Applies changed and explicitly removed messages atomically for one task.
   */
  updateMessages(taskId: string, messages: ChatMessageRow[], deleted: string[] = []): void {
    if (messages.some(message => message.task_id !== taskId)) throw new Error("Chat update spans multiple tasks");
    const transaction = this.db.transaction((msgs: ChatMessageRow[]) => {
      const remove = this.db.prepare("DELETE FROM messages WHERE task_id = ? AND id = ?");
      for (const id of deleted) remove.run(taskId, id);
      for (const msg of msgs) this.insertMessage(msg);
    });

    transaction(messages);
  }

  /**
   * Deletes all messages for a task.
   */
  deleteTaskMessages(taskId: string): void {
    this.db.prepare("DELETE FROM messages WHERE task_id = ?").run(taskId);
  }

  // ─── Read Operations ─────────────────────────────────────────────────────

  /**
   * Loads all messages for a task, ordered by timestamp.
   */
  loadMessages(taskId: string): ChatMessageRow[] {
    return this.db
      .prepare("SELECT * FROM messages WHERE task_id = ? ORDER BY timestamp ASC")
      .all(taskId) as ChatMessageRow[];
  }

  /**
   * Loads messages with pagination.
   */
  loadMessagesPaginated(taskId: string, limit: number, offset: number): ChatMessageRow[] {
    return this.db
      .prepare("SELECT * FROM messages WHERE task_id = ? ORDER BY timestamp ASC LIMIT ? OFFSET ?")
      .all(taskId, limit, offset) as ChatMessageRow[];
  }

  /**
   * Returns the count of messages for a task.
   */
  countMessages(taskId: string): number {
    const row = this.db
      .prepare("SELECT COUNT(*) as count FROM messages WHERE task_id = ?")
      .get(taskId) as { count: number } | undefined;
    return row?.count ?? 0;
  }

  // ─── Search ──────────────────────────────────────────────────────────────

  /**
   * Full-text search across all messages. Returns matching messages with
   * highlighted snippets.
   */
  search(query: string, limit = 50): SearchResult[] {
    if (!query.trim()) return [];

    return this.db
      .prepare(`
        SELECT
          m.id,
          m.task_id,
          m.role,
          m.content,
          m.timestamp,
          snippet(messages_fts, 0, '<mark>', '</mark>', '…', 40) as snippet
        FROM messages_fts
        JOIN messages m ON m.rowid = messages_fts.rowid
        WHERE messages_fts MATCH ?
        ORDER BY rank
        LIMIT ?
      `)
      .all(query, limit) as SearchResult[];
  }

  /**
   * Search within a specific task's messages.
   */
  searchInTask(taskId: string, query: string, limit = 50): SearchResult[] {
    if (!query.trim()) return [];

    return this.db
      .prepare(`
        SELECT
          m.id,
          m.task_id,
          m.role,
          m.content,
          m.timestamp,
          snippet(messages_fts, 0, '<mark>', '</mark>', '…', 40) as snippet
        FROM messages_fts
        JOIN messages m ON m.rowid = messages_fts.rowid
        WHERE messages_fts MATCH ? AND m.task_id = ?
        ORDER BY rank
        LIMIT ?
      `)
      .all(query, taskId, limit) as SearchResult[];
  }

  // ─── Context Retrieval ───────────────────────────────────────────────────

  /**
   * Gets the last N messages for a task (useful for building agent context).
   */
  getRecentMessages(taskId: string, limit = 20): ChatMessageRow[] {
    return this.db
      .prepare(`
        SELECT * FROM messages
        WHERE task_id = ?
        ORDER BY timestamp DESC
        LIMIT ?
      `)
      .all(taskId, limit)
      .reverse() as ChatMessageRow[];
  }

  /**
   * Gets messages of a specific kind (e.g., all tool events for a task).
   */
  getMessagesByKind(taskId: string, kind: string): ChatMessageRow[] {
    return this.db
      .prepare("SELECT * FROM messages WHERE task_id = ? AND kind = ? ORDER BY timestamp ASC")
      .all(taskId, kind) as ChatMessageRow[];
  }

  /**
   * Gets all task IDs that have stored messages.
   */
  getTaskIds(): string[] {
    const rows = this.db
      .prepare("SELECT DISTINCT task_id FROM messages ORDER BY MAX(timestamp) DESC")
      .all() as Array<{ task_id: string }>;
    return rows.map((r) => r.task_id);
  }

  // ─── Lifecycle ───────────────────────────────────────────────────────────

  /**
   * Closes the database connection. Call on app quit.
   */
  close(): void {
    this.db.close();
  }
}
