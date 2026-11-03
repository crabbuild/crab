/**
 * SQLite-backed skills metadata persistence.
 *
 * Stores skill records as an index on top of the canonical
 * SKILL.md directories on disk.
 */

import Database from "better-sqlite3";
import * as fs from "node:fs";
import * as path from "node:path";
import * as os from "node:os";

import type { SkillRecord, SkillScope, SkillActivationPrefs } from "./skill-manager-types";

const DB_DIR = path.join(os.homedir(), ".crab", "desktop");
const DB_PATH = path.join(DB_DIR, "skills-index.sqlite");

type SqliteStatement = {
  run: (...args: unknown[]) => unknown;
  get: (...args: unknown[]) => unknown;
  all: (...args: unknown[]) => unknown[];
};

type SqliteDatabase = {
  prepare: (sql: string) => SqliteStatement;
  exec: (sql: string) => void;
  transaction: <T extends (...args: unknown[]) => unknown>(fn: T) => T;
  close: () => void;
};

function createNodeSqliteDatabase(dbPath: string): SqliteDatabase {
  const { DatabaseSync } = require("node:sqlite") as {
    DatabaseSync: new (dbPath: string) => {
      prepare: (sql: string) => SqliteStatement;
      exec: (sql: string) => void;
      close: () => void;
    };
  };
  const db = new DatabaseSync(dbPath);

  return {
    prepare: (sql) => db.prepare(sql),
    exec: (sql) => db.exec(sql),
    close: () => db.close(),
    transaction: <T extends (...args: unknown[]) => unknown>(fn: T): T => {
      return ((...args: Parameters<T>): ReturnType<T> => {
        db.exec("BEGIN IMMEDIATE");
        try {
          const result = fn(...args) as ReturnType<T>;
          db.exec("COMMIT");
          return result;
        } catch (err) {
          try {
            db.exec("ROLLBACK");
          } catch {
            // Preserve the original transaction error.
          }
          throw err;
        }
      }) as T;
    },
  };
}

type SkillRow = {
  id: string;
  name: string;
  description: string;
  scope: string;
  repo_path: string | null;
  source_url: string | null;
  installed_at: number;
  updated_at: number;
  content_hash: string;
  enabled: number;
  auto_activate: number;
  restrict_to_agents: string;
};

function rowToRecord(row: SkillRow): SkillRecord {
  return {
    id: row.id,
    name: row.name,
    description: row.description,
    scope: row.scope as SkillScope,
    repoPath: row.repo_path,
    sourceUrl: row.source_url,
    installedAt: row.installed_at,
    updatedAt: row.updated_at,
    contentHash: row.content_hash,
    enabled: row.enabled === 1,
    activationPrefs: {
      autoActivate: row.auto_activate === 1,
      restrictToAgents: JSON.parse(row.restrict_to_agents) as string[],
    },
  };
}

export class SkillDatabase {
  private readonly db: SqliteDatabase;

  constructor(dbPath?: string) {
    const resolvedPath = dbPath ?? DB_PATH;
    fs.mkdirSync(path.dirname(resolvedPath), { recursive: true });
    try {
      this.db = new Database(resolvedPath) as unknown as SqliteDatabase;
    } catch {
      this.db = createNodeSqliteDatabase(resolvedPath);
    }
    this.db.exec(`
      PRAGMA journal_mode = WAL;
      PRAGMA synchronous = NORMAL;
      PRAGMA foreign_keys = ON;
    `);
    this.migrate();
  }

  close(): void {
    this.db.close();
  }

  upsertSkill(record: SkillRecord): void {
    this.db
      .prepare(
        `
      INSERT INTO skills (
        id, name, description, scope, repo_path, source_url,
        installed_at, updated_at, content_hash, enabled,
        auto_activate, restrict_to_agents
      ) VALUES (
        @id, @name, @description, @scope, @repo_path, @source_url,
        @installed_at, @updated_at, @content_hash, @enabled,
        @auto_activate, @restrict_to_agents
      )
      ON CONFLICT(id) DO UPDATE SET
        name = excluded.name,
        description = excluded.description,
        scope = excluded.scope,
        repo_path = excluded.repo_path,
        source_url = excluded.source_url,
        installed_at = excluded.installed_at,
        updated_at = excluded.updated_at,
        content_hash = excluded.content_hash,
        enabled = excluded.enabled,
        auto_activate = excluded.auto_activate,
        restrict_to_agents = excluded.restrict_to_agents
    `,
      )
      .run({
        id: record.id,
        name: record.name,
        description: record.description,
        scope: record.scope,
        repo_path: record.repoPath,
        source_url: record.sourceUrl,
        installed_at: record.installedAt,
        updated_at: record.updatedAt,
        content_hash: record.contentHash,
        enabled: record.enabled ? 1 : 0,
        auto_activate: record.activationPrefs.autoActivate ? 1 : 0,
        restrict_to_agents: JSON.stringify(
          record.activationPrefs.restrictToAgents,
        ),
      });
  }

  deleteSkill(id: string): void {
    this.db.prepare("DELETE FROM skills WHERE id = ?").run(id);
  }

  getSkill(id: string): SkillRecord | null {
    const row = this.db
      .prepare("SELECT * FROM skills WHERE id = ?")
      .get(id) as SkillRow | undefined;
    if (!row) return null;
    return rowToRecord(row);
  }

  listSkills(opts?: {
    scope?: SkillScope;
    repoPath?: string;
  }): SkillRecord[] {
    if (opts?.scope === "repo" && opts.repoPath) {
      const rows = this.db
        .prepare(
          "SELECT * FROM skills WHERE scope = 'repo' AND repo_path = ? ORDER BY name ASC",
        )
        .all(opts.repoPath) as SkillRow[];
      return rows.map(rowToRecord);
    }
    if (opts?.scope === "user") {
      const rows = this.db
        .prepare(
          "SELECT * FROM skills WHERE scope = 'user' ORDER BY name ASC",
        )
        .all() as SkillRow[];
      return rows.map(rowToRecord);
    }
    const rows = this.db
      .prepare("SELECT * FROM skills ORDER BY scope, name ASC")
      .all() as SkillRow[];
    return rows.map(rowToRecord);
  }

  searchSkills(query: string): SkillRecord[] {
    const rows = this.db
      .prepare(
        "SELECT * FROM skills WHERE name LIKE @q OR description LIKE @q ORDER BY name ASC",
      )
      .all({ q: `%${query}%` }) as SkillRow[];
    return rows.map(rowToRecord);
  }

  setEnabled(id: string, enabled: boolean): void {
    this.db
      .prepare("UPDATE skills SET enabled = ? WHERE id = ?")
      .run(enabled ? 1 : 0, id);
  }

  setActivationPrefs(id: string, prefs: SkillActivationPrefs): void {
    this.db
      .prepare(
        "UPDATE skills SET auto_activate = ?, restrict_to_agents = ? WHERE id = ?",
      )
      .run(
        prefs.autoActivate ? 1 : 0,
        JSON.stringify(prefs.restrictToAgents),
        id,
      );
  }

  getSkillIdsForScope(
    scope: SkillScope,
    repoPath?: string,
  ): Set<string> {
    let rows: Array<{ id: string }>;
    if (scope === "repo" && repoPath) {
      rows = this.db
        .prepare(
          "SELECT id FROM skills WHERE scope = 'repo' AND repo_path = ?",
        )
        .all(repoPath) as Array<{ id: string }>;
    } else {
      rows = this.db
        .prepare("SELECT id FROM skills WHERE scope = ?")
        .all(scope) as Array<{ id: string }>;
    }
    return new Set(rows.map((r) => r.id));
  }

  private migrate(): void {
    this.db.exec(`
      CREATE TABLE IF NOT EXISTS skills (
        id TEXT PRIMARY KEY,
        name TEXT NOT NULL,
        description TEXT NOT NULL DEFAULT '',
        scope TEXT NOT NULL CHECK(scope IN ('user','repo')),
        repo_path TEXT,
        source_url TEXT,
        installed_at INTEGER NOT NULL,
        updated_at INTEGER NOT NULL,
        content_hash TEXT NOT NULL,
        enabled INTEGER NOT NULL DEFAULT 1,
        auto_activate INTEGER NOT NULL DEFAULT 0,
        restrict_to_agents TEXT NOT NULL DEFAULT '[]'
      );

      CREATE INDEX IF NOT EXISTS idx_skills_scope ON skills(scope);
      CREATE INDEX IF NOT EXISTS idx_skills_repo ON skills(repo_path);
    `);
  }
}
