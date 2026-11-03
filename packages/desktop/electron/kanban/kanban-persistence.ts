/**
 * Atomic JSON persistence for the Kanban subsystem.
 *
 * All writes use a temp-then-rename pattern to prevent corruption on crash.
 * Each file carries a `version` field; on load, older versions are migrated
 * forward. If a file is corrupted (invalid JSON or failed validation), it is
 * renamed to `.bak` and defaults are returned.
 */

import * as fs from "node:fs/promises";
import * as fsSync from "node:fs";
import * as path from "node:path";
import * as os from "node:os";
import { randomUUID } from "node:crypto";
import Database from "better-sqlite3";
import log from "electron-log";

import type {
  BoardConfig,
  KanbanColumn,
  KanbanCard,
  AutomationRule,
  CardAnalytics,
  BoardFieldDefinition,
  KanbanTaskActivity,
  KanbanTaskRun,
  RelationType,
  PersistedBoardData,
  PersistedRuleData,
  PersistedAnalyticsData,
  PersistedBoardState,
} from "./kanban-types";

// ── Atomic I/O ───────────────────────────────────────────────────

/**
 * Writes JSON data atomically using write-to-temp-then-rename.
 * The temp file uses the process PID as a suffix to avoid collisions.
 */
export async function atomicWrite(filePath: string, data: unknown): Promise<void> {
  const dir = path.dirname(filePath);
  await fs.mkdir(dir, { recursive: true });

  const tmpPath = `${filePath}.tmp.${process.pid}`;
  const content = JSON.stringify(data, null, 2) + "\n";

  await fs.writeFile(tmpPath, content, "utf-8");
  await fs.rename(tmpPath, filePath);
}

/**
 * Reads and parses a JSON file with version checking and migration support.
 *
 * On success, returns the parsed and (possibly migrated) data.
 * On missing file, returns `null`.
 * On corruption (invalid JSON or failed validation), renames the file to `.bak`,
 * logs a warning, and returns `null` so the caller can apply defaults.
 */
export async function atomicRead<T>(
  filePath: string,
  validate: (raw: unknown) => T,
  migrate: (raw: Record<string, unknown>) => Record<string, unknown>,
): Promise<T | null> {
  let raw: string;
  try {
    raw = await fs.readFile(filePath, "utf-8");
  } catch (err: unknown) {
    if (isNodeError(err) && err.code === "ENOENT") {
      return null;
    }
    return null;
  }

  let parsed: unknown;
  try {
    parsed = JSON.parse(raw);
  } catch {
    await handleCorruption(filePath, "invalid JSON");
    return null;
  }

  try {
    if (typeof parsed === "object" && parsed !== null) {
      parsed = migrate(parsed as Record<string, unknown>);
    }
    return validate(parsed);
  } catch (err: unknown) {
    const msg = err instanceof Error ? err.message : String(err);
    await handleCorruption(filePath, msg);
    return null;
  }
}

async function handleCorruption(filePath: string, reason: string): Promise<void> {
  const bakPath = `${filePath}.bak`;
  try {
    await fs.rename(filePath, bakPath);
  } catch {
    // If rename fails (e.g., file already gone), just log and continue
  }
  log.warn(
    `[kanban-persistence] Corrupted file renamed to .bak: ${filePath} (${reason})`,
  );
}

// ── Schema Migration Framework ───────────────────────────────────

type MigrationFn = (data: Record<string, unknown>) => Record<string, unknown>;

interface MigrationRegistry {
  [fromVersion: number]: MigrationFn;
}

/**
 * Applies sequential migrations from the file's current version up to the
 * target version. Each migration function transforms from version N to N+1.
 */
function applyMigrations(
  data: Record<string, unknown>,
  targetVersion: number,
  migrations: MigrationRegistry,
): Record<string, unknown> {
  let current = typeof data.version === "number" ? data.version : 0;
  let result = { ...data };

  while (current < targetVersion) {
    const migrateFn = migrations[current];
    if (!migrateFn) {
      throw new Error(
        `No migration path from version ${current} to ${current + 1}`,
      );
    }
    result = migrateFn(result);
    current++;
    result.version = current;
  }

  return result;
}

// ── Board Migrations ─────────────────────────────────────────────

const BOARD_CURRENT_VERSION = 1;

const boardMigrations: MigrationRegistry = {
  // v0 → v1: initial schema, wrap bare array into versioned envelope
  0: (data) => {
    if (Array.isArray(data)) {
      return { version: 1, boards: data };
    }
    return { ...data, version: 1, boards: data.boards ?? [] };
  },
};

function migrateBoardData(data: Record<string, unknown>): Record<string, unknown> {
  return applyMigrations(data, BOARD_CURRENT_VERSION, boardMigrations);
}

// ── Rule Migrations ──────────────────────────────────────────────

const RULE_CURRENT_VERSION = 1;

const ruleMigrations: MigrationRegistry = {
  0: (data) => {
    if (Array.isArray(data)) {
      return { version: 1, rules: data };
    }
    return { ...data, version: 1, rules: data.rules ?? [] };
  },
};

function migrateRuleData(data: Record<string, unknown>): Record<string, unknown> {
  return applyMigrations(data, RULE_CURRENT_VERSION, ruleMigrations);
}

// ── Analytics Migrations ─────────────────────────────────────────

const ANALYTICS_CURRENT_VERSION = 1;

const analyticsMigrations: MigrationRegistry = {
  0: (data) => {
    if (Array.isArray(data)) {
      return { version: 1, analytics: data };
    }
    return { ...data, version: 1, analytics: data.analytics ?? [] };
  },
};

function migrateAnalyticsData(data: Record<string, unknown>): Record<string, unknown> {
  return applyMigrations(data, ANALYTICS_CURRENT_VERSION, analyticsMigrations);
}

// ── State Migrations ─────────────────────────────────────────────

const STATE_CURRENT_VERSION = 1;

const stateMigrations: MigrationRegistry = {
  0: (data) => {
    if (Array.isArray(data)) {
      return { version: 1, cards: data };
    }
    return { ...data, version: 1, cards: data.cards ?? [] };
  },
};

function migrateStateData(data: Record<string, unknown>): Record<string, unknown> {
  return applyMigrations(data, STATE_CURRENT_VERSION, stateMigrations);
}

// ── Validators ───────────────────────────────────────────────────

function validateBoardData(raw: unknown): PersistedBoardData {
  if (typeof raw !== "object" || raw === null) {
    throw new Error("Board data is not an object");
  }
  const obj = raw as Record<string, unknown>;
  if (obj.version !== BOARD_CURRENT_VERSION) {
    throw new Error(`Unsupported board data version: ${obj.version}`);
  }
  if (!Array.isArray(obj.boards)) {
    throw new Error("Missing boards array");
  }
  return obj as unknown as PersistedBoardData;
}

function validateRuleData(raw: unknown): PersistedRuleData {
  if (typeof raw !== "object" || raw === null) {
    throw new Error("Rule data is not an object");
  }
  const obj = raw as Record<string, unknown>;
  if (obj.version !== RULE_CURRENT_VERSION) {
    throw new Error(`Unsupported rule data version: ${obj.version}`);
  }
  if (!Array.isArray(obj.rules)) {
    throw new Error("Missing rules array");
  }
  return obj as unknown as PersistedRuleData;
}

function validateAnalyticsData(raw: unknown): PersistedAnalyticsData {
  if (typeof raw !== "object" || raw === null) {
    throw new Error("Analytics data is not an object");
  }
  const obj = raw as Record<string, unknown>;
  if (obj.version !== ANALYTICS_CURRENT_VERSION) {
    throw new Error(`Unsupported analytics data version: ${obj.version}`);
  }
  if (!Array.isArray(obj.analytics)) {
    throw new Error("Missing analytics array");
  }
  return obj as unknown as PersistedAnalyticsData;
}

function validateStateData(raw: unknown): PersistedBoardState {
  if (typeof raw !== "object" || raw === null) {
    throw new Error("State data is not an object");
  }
  const obj = raw as Record<string, unknown>;
  if (obj.version !== STATE_CURRENT_VERSION) {
    throw new Error(`Unsupported state data version: ${obj.version}`);
  }
  if (!Array.isArray(obj.cards)) {
    throw new Error("Missing cards array");
  }
  return obj as unknown as PersistedBoardState;
}

// ── Default Data ─────────────────────────────────────────────────

const DEFAULT_COLUMNS: KanbanColumn[] = [
  { id: "backlog", name: "Backlog", trackerStatusMapping: ["backlog", "open"] },
  { id: "ready", name: "Ready", trackerStatusMapping: ["ready", "todo"] },
  { id: "in-progress", name: "In Progress", trackerStatusMapping: ["in progress", "in_progress"] },
  { id: "in-review", name: "In Review", trackerStatusMapping: ["in review", "review"] },
  { id: "done", name: "Done", trackerStatusMapping: ["done", "closed"] },
];

// ── File Paths ───────────────────────────────────────────────────

function kanbanDir(): string {
  return path.join(os.homedir(), ".crab", "desktop");
}

function boardsFilePath(): string {
  return path.join(kanbanDir(), "kanban-boards.json");
}

function rulesFilePath(): string {
  return path.join(kanbanDir(), "kanban-rules.json");
}

function analyticsFilePath(): string {
  return path.join(kanbanDir(), "kanban-analytics.json");
}

function stateFilePath(): string {
  return path.join(kanbanDir(), "kanban-state.json");
}

function kanbanDbPath(): string {
  return path.join(kanbanDir(), "kanban.db");
}

function sqlitePathForLegacyFile(filePath: string | undefined): string {
  if (!filePath) return kanbanDbPath();
  return `${filePath}.sqlite`;
}

function defaultLegacyImportPaths(): LegacyImportPaths {
  return {
    boards: boardsFilePath(),
    rules: rulesFilePath(),
    analytics: analyticsFilePath(),
    state: stateFilePath(),
  };
}

interface LegacyImportPaths {
  boards?: string;
  rules?: string;
  analytics?: string;
  state?: string;
}

interface SqliteRow<T = string> {
  [key: string]: T;
}

function encodeJson(value: unknown): string {
  return JSON.stringify(value ?? null);
}

function decodeJson<T>(value: string | null | undefined, fallback: T): T {
  if (!value) return fallback;
  try {
    return JSON.parse(value) as T;
  } catch {
    return fallback;
  }
}

function nowMs(): number {
  return Date.now();
}

/**
 * SQLite persistence for kanban boards, tasks, attributes, relations, runs,
 * activity, rules, and analytics. Public store classes below keep the older
 * async JSON-store shape while using this database as the canonical source.
 */
export class KanbanDatabase {
  private db: Database.Database;

  constructor(
    dbPath = kanbanDbPath(),
    private readonly importPaths: LegacyImportPaths = defaultLegacyImportPaths(),
  ) {
    fsSync.mkdirSync(path.dirname(dbPath), { recursive: true });
    this.db = new Database(dbPath);
    this.db.pragma("journal_mode = WAL");
    this.db.pragma("synchronous = NORMAL");
    this.migrate();
    this.importLegacyJson();
  }

  close(): void {
    this.db.close();
  }

  private migrate(): void {
    this.db.exec(`
      CREATE TABLE IF NOT EXISTS meta (
        key TEXT PRIMARY KEY,
        value TEXT NOT NULL
      );

      CREATE TABLE IF NOT EXISTS boards (
        id TEXT PRIMARY KEY,
        data TEXT NOT NULL,
        created_at INTEGER NOT NULL,
        updated_at INTEGER NOT NULL
      );

      CREATE TABLE IF NOT EXISTS board_columns (
        board_id TEXT NOT NULL,
        column_id TEXT NOT NULL,
        position INTEGER NOT NULL,
        data TEXT NOT NULL,
        PRIMARY KEY (board_id, column_id)
      );

      CREATE TABLE IF NOT EXISTS board_fields (
        board_id TEXT NOT NULL,
        field_id TEXT NOT NULL,
        position INTEGER NOT NULL,
        data TEXT NOT NULL,
        PRIMARY KEY (board_id, field_id)
      );

      CREATE TABLE IF NOT EXISTS board_field_values (
        board_id TEXT NOT NULL,
        field_id TEXT NOT NULL,
        value_json TEXT NOT NULL,
        PRIMARY KEY (board_id, field_id)
      );

      CREATE TABLE IF NOT EXISTS tasks (
        id TEXT PRIMARY KEY,
        board_id TEXT,
        column_id TEXT,
        position_index INTEGER NOT NULL DEFAULT 0,
        data TEXT NOT NULL,
        created_at INTEGER NOT NULL,
        updated_at INTEGER NOT NULL,
        archived_at INTEGER
      );

      CREATE INDEX IF NOT EXISTS idx_tasks_board
        ON tasks(board_id, position_index);

      CREATE TABLE IF NOT EXISTS task_field_values (
        task_id TEXT NOT NULL,
        field_id TEXT NOT NULL,
        value_json TEXT NOT NULL,
        PRIMARY KEY (task_id, field_id)
      );

      CREATE TABLE IF NOT EXISTS task_relations (
        source_task_id TEXT NOT NULL,
        target_task_id TEXT NOT NULL,
        type TEXT NOT NULL,
        created_at INTEGER NOT NULL,
        PRIMARY KEY (source_task_id, target_task_id, type)
      );

      CREATE TABLE IF NOT EXISTS task_runs (
        id TEXT PRIMARY KEY,
        task_id TEXT NOT NULL,
        board_id TEXT NOT NULL,
        orchestrator_task_id TEXT NOT NULL,
        status TEXT NOT NULL,
        worktree_path TEXT,
        branch TEXT,
        base_ref TEXT,
        agent_type TEXT,
        model TEXT,
        reasoning_level TEXT,
        mode TEXT,
        summary TEXT,
        created_at INTEGER NOT NULL,
        started_at INTEGER,
        completed_at INTEGER
      );

      CREATE INDEX IF NOT EXISTS idx_task_runs_task
        ON task_runs(task_id, created_at DESC);

      CREATE INDEX IF NOT EXISTS idx_task_runs_orchestrator
        ON task_runs(orchestrator_task_id);

      CREATE TABLE IF NOT EXISTS task_activity (
        id TEXT PRIMARY KEY,
        task_id TEXT NOT NULL,
        board_id TEXT NOT NULL,
        type TEXT NOT NULL,
        message TEXT NOT NULL,
        field TEXT,
        from_value_json TEXT,
        to_value_json TEXT,
        created_at INTEGER NOT NULL
      );

      CREATE INDEX IF NOT EXISTS idx_task_activity_task
        ON task_activity(task_id, created_at DESC);

      CREATE TABLE IF NOT EXISTS automation_rules (
        id TEXT PRIMARY KEY,
        data TEXT NOT NULL,
        updated_at INTEGER NOT NULL
      );

      CREATE TABLE IF NOT EXISTS card_analytics (
        card_id TEXT PRIMARY KEY,
        data TEXT NOT NULL,
        updated_at INTEGER NOT NULL
      );
    `);

    this.db
      .prepare("INSERT OR REPLACE INTO meta (key, value) VALUES ('schema_version', '1')")
      .run();
  }

  private importLegacyJson(): void {
    this.importLegacyFile<PersistedBoardData>(
      "boards",
      this.importPaths.boards,
      validateBoardData,
      migrateBoardData,
      (data) => {
        if (this.countRows("boards") > 0) return;
        for (const board of data.boards) this.upsertBoard(board);
      },
    );
    this.importLegacyFile<PersistedBoardState>(
      "state",
      this.importPaths.state,
      validateStateData,
      migrateStateData,
      (data) => {
        if (this.countRows("tasks") > 0) return;
        this.replaceAllTasks(data.cards);
      },
    );
    this.importLegacyFile<PersistedRuleData>(
      "rules",
      this.importPaths.rules,
      validateRuleData,
      migrateRuleData,
      (data) => {
        if (this.countRows("automation_rules") > 0) return;
        for (const rule of data.rules) this.upsertRule(rule);
      },
    );
    this.importLegacyFile<PersistedAnalyticsData>(
      "analytics",
      this.importPaths.analytics,
      validateAnalyticsData,
      migrateAnalyticsData,
      (data) => {
        if (this.countRows("card_analytics") > 0) return;
        for (const analytics of data.analytics) this.upsertAnalytics(analytics);
      },
    );
  }

  private importLegacyFile<T>(
    kind: keyof LegacyImportPaths,
    filePath: string | undefined,
    validate: (raw: unknown) => T,
    migrate: (raw: Record<string, unknown>) => Record<string, unknown>,
    apply: (data: T) => void,
  ): void {
    if (!filePath) return;
    const metaKey = `legacy_imported:${kind}:${filePath}`;
    const imported = this.db
      .prepare("SELECT value FROM meta WHERE key = ?")
      .get(metaKey) as { value: string } | undefined;
    if (imported?.value === "1") return;
    if (!fsSync.existsSync(filePath)) {
      this.db.prepare("INSERT OR REPLACE INTO meta (key, value) VALUES (?, '1')").run(metaKey);
      return;
    }

    try {
      const raw = fsSync.readFileSync(filePath, "utf-8");
      let parsed: unknown = JSON.parse(raw);
      if (typeof parsed === "object" && parsed !== null) {
        parsed = migrate(parsed as Record<string, unknown>);
      }
      const data = validate(parsed);
      const tx = this.db.transaction(() => apply(data));
      tx();
      this.db.prepare("INSERT OR REPLACE INTO meta (key, value) VALUES (?, '1')").run(metaKey);
    } catch (err) {
      try {
        fsSync.renameSync(filePath, `${filePath}.bak`);
      } catch {
        // Best-effort corruption handling.
      }
      log.warn(
        `[kanban-persistence] Failed to import legacy ${kind} file ${filePath}: ${
          err instanceof Error ? err.message : String(err)
        }`,
      );
    }
  }

  private countRows(table: string): number {
    const row = this.db.prepare(`SELECT COUNT(*) as count FROM ${table}`).get() as { count: number };
    return row.count;
  }

  getBoards(): BoardConfig[] {
    const rows = this.db
      .prepare("SELECT data FROM boards ORDER BY updated_at DESC")
      .all() as Array<{ data: string }>;
    return rows.map((row) => this.hydrateBoard(decodeJson<BoardConfig>(row.data, {} as BoardConfig)));
  }

  getBoardById(boardId: string): BoardConfig | undefined {
    const row = this.db.prepare("SELECT data FROM boards WHERE id = ?").get(boardId) as { data: string } | undefined;
    return row ? this.hydrateBoard(decodeJson<BoardConfig>(row.data, {} as BoardConfig)) : undefined;
  }

  saveBoards(boards: BoardConfig[]): void {
    const tx = this.db.transaction((items: BoardConfig[]) => {
      const nextIds = new Set(items.map((board) => board.id));
      const existing = this.db.prepare("SELECT id FROM boards").all() as Array<{ id: string }>;
      for (const row of existing) {
        if (!nextIds.has(row.id)) this.deleteBoard(row.id);
      }
      for (const board of items) this.upsertBoard(board);
    });
    tx(boards);
  }

  upsertBoard(board: BoardConfig): void {
    const now = nowMs();
    const existing = this.getBoardById(board.id);
    const existingWithTimestamps = existing as (BoardConfig & { createdAt?: number }) | undefined;
    const boardWithTimestamps = board as BoardConfig & { updatedAt?: number };
    const createdAt = existingWithTimestamps?.createdAt ?? now;
    const updatedAt = boardWithTimestamps.updatedAt ?? now;
    const tx = this.db.transaction((item: BoardConfig) => {
      this.db
        .prepare("INSERT OR REPLACE INTO boards (id, data, created_at, updated_at) VALUES (?, ?, ?, ?)")
        .run(item.id, encodeJson(item), createdAt, updatedAt);

      this.db.prepare("DELETE FROM board_columns WHERE board_id = ?").run(item.id);
      const insertColumn = this.db.prepare(
        "INSERT INTO board_columns (board_id, column_id, position, data) VALUES (?, ?, ?, ?)",
      );
      (item.columns ?? []).forEach((column, index) => {
        insertColumn.run(item.id, column.id, index, encodeJson(column));
      });

      this.db.prepare("DELETE FROM board_fields WHERE board_id = ?").run(item.id);
      const insertField = this.db.prepare(
        "INSERT INTO board_fields (board_id, field_id, position, data) VALUES (?, ?, ?, ?)",
      );
      (item.fields ?? []).forEach((field, index) => {
        insertField.run(item.id, field.id, index, encodeJson(field));
      });

      this.db.prepare("DELETE FROM board_field_values WHERE board_id = ?").run(item.id);
      const insertValue = this.db.prepare(
        "INSERT INTO board_field_values (board_id, field_id, value_json) VALUES (?, ?, ?)",
      );
      for (const [fieldId, value] of Object.entries(item.customFields ?? {})) {
        insertValue.run(item.id, fieldId, encodeJson(value));
      }
    });
    tx(board);
  }

  deleteBoard(boardId: string): void {
    const tx = this.db.transaction(() => {
      this.db.prepare("DELETE FROM boards WHERE id = ?").run(boardId);
      this.db.prepare("DELETE FROM board_columns WHERE board_id = ?").run(boardId);
      this.db.prepare("DELETE FROM board_fields WHERE board_id = ?").run(boardId);
      this.db.prepare("DELETE FROM board_field_values WHERE board_id = ?").run(boardId);
      const taskRows = this.db.prepare("SELECT id FROM tasks WHERE board_id = ?").all(boardId) as Array<{ id: string }>;
      for (const row of taskRows) this.deleteTask(row.id);
    });
    tx();
  }

  private hydrateBoard(board: BoardConfig): BoardConfig {
    const columns = this.db
      .prepare("SELECT data FROM board_columns WHERE board_id = ? ORDER BY position ASC")
      .all(board.id) as Array<{ data: string }>;
    const fields = this.db
      .prepare("SELECT data FROM board_fields WHERE board_id = ? ORDER BY position ASC")
      .all(board.id) as Array<{ data: string }>;
    const values = this.db
      .prepare("SELECT field_id, value_json FROM board_field_values WHERE board_id = ?")
      .all(board.id) as Array<{ field_id: string; value_json: string }>;
    const customFields: Record<string, unknown> = {};
    for (const row of values) customFields[row.field_id] = decodeJson<unknown>(row.value_json, null);
    const hydrated: BoardConfig = {
      ...board,
      columns: columns.map((row) => decodeJson<KanbanColumn>(row.data, {} as KanbanColumn)).filter((col) => col.id),
    };
    const hydratedFields = fields
      .map((row) => decodeJson<BoardFieldDefinition>(row.data, {} as BoardFieldDefinition))
      .filter((field) => field.id);
    if (hydratedFields.length > 0 || board.fields) hydrated.fields = hydratedFields;
    if (Object.keys(customFields).length > 0 || board.customFields) hydrated.customFields = customFields;
    return hydrated;
  }

  getTasks(): KanbanCard[] {
    const rows = this.db
      .prepare("SELECT data FROM tasks ORDER BY updated_at DESC")
      .all() as Array<{ data: string }>;
    return rows.map((row) => this.hydrateTask(decodeJson<KanbanCard>(row.data, {} as KanbanCard)));
  }

  getTask(taskId: string): KanbanCard | undefined {
    const row = this.db.prepare("SELECT data FROM tasks WHERE id = ?").get(taskId) as { data: string } | undefined;
    return row ? this.hydrateTask(decodeJson<KanbanCard>(row.data, {} as KanbanCard)) : undefined;
  }

  getTaskByOrchestratorTask(orchestratorTaskId: string): KanbanCard | undefined {
    const row = this.db
      .prepare("SELECT task_id FROM task_runs WHERE orchestrator_task_id = ? ORDER BY created_at DESC LIMIT 1")
      .get(orchestratorTaskId) as { task_id: string } | undefined;
    return row ? this.getTask(row.task_id) : undefined;
  }

  replaceAllTasks(tasks: KanbanCard[]): void {
    const tx = this.db.transaction((items: KanbanCard[]) => {
      const nextIds = new Set(items.map((task) => task.id));
      const existing = this.db.prepare("SELECT id FROM tasks").all() as Array<{ id: string }>;
      for (const row of existing) {
        if (!nextIds.has(row.id)) this.deleteTask(row.id);
      }
      for (const task of items) this.upsertTask(task);
    });
    tx(tasks);
  }

  upsertTask(task: KanbanCard): void {
    const now = nowMs();
    const createdAt = task.createdAt ?? now;
    const updatedAt = task.updatedAt ?? now;
    const tx = this.db.transaction((item: KanbanCard) => {
      this.db
        .prepare(
          `INSERT OR REPLACE INTO tasks
            (id, board_id, column_id, position_index, data, created_at, updated_at, archived_at)
           VALUES (?, ?, ?, ?, ?, ?, ?, ?)`,
        )
        .run(
          item.id,
          item.boardId ?? null,
          item.columnId ?? null,
          item.positionIndex ?? 0,
          encodeJson(stripTaskForStorage(item)),
          createdAt,
          updatedAt,
          item.archivedAt ?? null,
        );

      this.db.prepare("DELETE FROM task_field_values WHERE task_id = ?").run(item.id);
      const insertValue = this.db.prepare(
        "INSERT INTO task_field_values (task_id, field_id, value_json) VALUES (?, ?, ?)",
      );
      for (const [fieldId, value] of Object.entries(item.customFields ?? {})) {
        insertValue.run(item.id, fieldId, encodeJson(value));
      }

      this.db.prepare("DELETE FROM task_relations WHERE source_task_id = ?").run(item.id);
      const insertRelation = this.db.prepare(
        "INSERT OR REPLACE INTO task_relations (source_task_id, target_task_id, type, created_at) VALUES (?, ?, ?, ?)",
      );
      for (const relation of item.relations ?? []) {
        insertRelation.run(item.id, relation.targetCardId, relation.type, now);
      }
    });
    tx(task);
  }

  deleteTask(taskId: string): void {
    const tx = this.db.transaction(() => {
      this.db.prepare("DELETE FROM tasks WHERE id = ?").run(taskId);
      this.db.prepare("DELETE FROM task_field_values WHERE task_id = ?").run(taskId);
      this.db.prepare("DELETE FROM task_relations WHERE source_task_id = ? OR target_task_id = ?").run(taskId, taskId);
      this.db.prepare("DELETE FROM task_runs WHERE task_id = ?").run(taskId);
      this.db.prepare("DELETE FROM task_activity WHERE task_id = ?").run(taskId);
    });
    tx();
  }

  private hydrateTask(task: KanbanCard): KanbanCard {
    const values = this.db
      .prepare("SELECT field_id, value_json FROM task_field_values WHERE task_id = ?")
      .all(task.id) as Array<{ field_id: string; value_json: string }>;
    const customFields: Record<string, unknown> = { ...(task.customFields ?? {}) };
    for (const row of values) customFields[row.field_id] = decodeJson<unknown>(row.value_json, null);

    const relations = this.db
      .prepare("SELECT target_task_id, type FROM task_relations WHERE source_task_id = ?")
      .all(task.id) as Array<{ target_task_id: string; type: string }>;

    const hydrated: KanbanCard = {
      ...task,
      relations: relations.length > 0
        ? relations.map((row) => ({ targetCardId: row.target_task_id, type: row.type as RelationType }))
        : task.relations,
    };
    if (Object.keys(customFields).length > 0 || task.customFields) hydrated.customFields = customFields;
    const latestRun = this.getLatestRun(task.id);
    if (latestRun) hydrated.latestRun = latestRun;
    const runs = this.getRuns(task.id).slice(0, 5);
    if (runs.length > 0 || task.runs) hydrated.runs = runs;
    const activity = this.getActivity(task.id).slice(0, 20);
    if (activity.length > 0 || task.activity) hydrated.activity = activity;
    return hydrated;
  }

  addActivity(entry: Omit<KanbanTaskActivity, "id" | "createdAt"> & { id?: string; createdAt?: number }): KanbanTaskActivity {
    const activity: KanbanTaskActivity = {
      id: entry.id ?? randomUUID(),
      taskId: entry.taskId,
      boardId: entry.boardId,
      type: entry.type,
      message: entry.message,
      field: entry.field,
      fromValue: entry.fromValue,
      toValue: entry.toValue,
      createdAt: entry.createdAt ?? nowMs(),
    };
    this.db
      .prepare(
        `INSERT OR REPLACE INTO task_activity
          (id, task_id, board_id, type, message, field, from_value_json, to_value_json, created_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)`,
      )
      .run(
        activity.id,
        activity.taskId,
        activity.boardId,
        activity.type,
        activity.message,
        activity.field ?? null,
        activity.fromValue === undefined ? null : encodeJson(activity.fromValue),
        activity.toValue === undefined ? null : encodeJson(activity.toValue),
        activity.createdAt,
      );
    return activity;
  }

  getActivity(taskId: string): KanbanTaskActivity[] {
    const rows = this.db
      .prepare("SELECT * FROM task_activity WHERE task_id = ? ORDER BY created_at DESC")
      .all(taskId) as Array<SqliteRow<unknown>>;
    return rows.map((row) => ({
      id: String(row.id),
      taskId: String(row.task_id),
      boardId: String(row.board_id),
      type: row.type as KanbanTaskActivity["type"],
      message: String(row.message),
      field: typeof row.field === "string" ? row.field : undefined,
      fromValue: typeof row.from_value_json === "string" ? decodeJson<unknown>(row.from_value_json, null) : undefined,
      toValue: typeof row.to_value_json === "string" ? decodeJson<unknown>(row.to_value_json, null) : undefined,
      createdAt: Number(row.created_at),
    }));
  }

  addTaskRun(run: KanbanTaskRun): KanbanTaskRun {
    this.db
      .prepare(
        `INSERT OR REPLACE INTO task_runs
          (id, task_id, board_id, orchestrator_task_id, status, worktree_path, branch, base_ref,
           agent_type, model, reasoning_level, mode, summary, created_at, started_at, completed_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)`,
      )
      .run(
        run.id,
        run.taskId,
        run.boardId,
        run.orchestratorTaskId,
        run.status,
        run.worktreePath ?? null,
        run.branch ?? null,
        run.baseRef ?? null,
        run.agentType ?? null,
        run.model ?? null,
        run.reasoningLevel ?? null,
        run.mode ?? null,
        run.summary ?? null,
        run.createdAt,
        run.startedAt ?? null,
        run.completedAt ?? null,
      );
    return run;
  }

  updateTaskRun(runId: string, updates: Partial<KanbanTaskRun>): KanbanTaskRun | null {
    const existing = this.getRunById(runId);
    if (!existing) return null;
    const next = { ...existing, ...updates };
    return this.addTaskRun(next);
  }

  getRuns(taskId: string): KanbanTaskRun[] {
    const rows = this.db
      .prepare("SELECT * FROM task_runs WHERE task_id = ? ORDER BY created_at DESC")
      .all(taskId) as Array<SqliteRow<unknown>>;
    return rows.map(rowToTaskRun);
  }

  getLatestRun(taskId: string): KanbanTaskRun | undefined {
    const row = this.db
      .prepare("SELECT * FROM task_runs WHERE task_id = ? ORDER BY created_at DESC LIMIT 1")
      .get(taskId) as SqliteRow<unknown> | undefined;
    return row ? rowToTaskRun(row) : undefined;
  }

  getRunById(runId: string): KanbanTaskRun | undefined {
    const row = this.db.prepare("SELECT * FROM task_runs WHERE id = ?").get(runId) as SqliteRow<unknown> | undefined;
    return row ? rowToTaskRun(row) : undefined;
  }

  getRunByOrchestratorTask(orchestratorTaskId: string): KanbanTaskRun | undefined {
    const row = this.db
      .prepare("SELECT * FROM task_runs WHERE orchestrator_task_id = ? ORDER BY created_at DESC LIMIT 1")
      .get(orchestratorTaskId) as SqliteRow<unknown> | undefined;
    return row ? rowToTaskRun(row) : undefined;
  }

  getRules(): AutomationRule[] {
    const rows = this.db.prepare("SELECT data FROM automation_rules ORDER BY updated_at DESC").all() as Array<{ data: string }>;
    return rows.map((row) => decodeJson<AutomationRule>(row.data, {} as AutomationRule)).filter((rule) => rule.id);
  }

  upsertRule(rule: AutomationRule): void {
    this.db
      .prepare("INSERT OR REPLACE INTO automation_rules (id, data, updated_at) VALUES (?, ?, ?)")
      .run(rule.id, encodeJson(rule), nowMs());
  }

  replaceRules(rules: AutomationRule[]): void {
    const tx = this.db.transaction((items: AutomationRule[]) => {
      this.db.prepare("DELETE FROM automation_rules").run();
      for (const rule of items) this.upsertRule(rule);
    });
    tx(rules);
  }

  deleteRule(ruleId: string): void {
    this.db.prepare("DELETE FROM automation_rules WHERE id = ?").run(ruleId);
  }

  getAnalytics(): CardAnalytics[] {
    const rows = this.db.prepare("SELECT data FROM card_analytics ORDER BY updated_at DESC").all() as Array<{ data: string }>;
    return rows.map((row) => decodeJson<CardAnalytics>(row.data, {} as CardAnalytics)).filter((item) => item.cardId);
  }

  getCardAnalytics(cardId: string): CardAnalytics | undefined {
    const row = this.db.prepare("SELECT data FROM card_analytics WHERE card_id = ?").get(cardId) as { data: string } | undefined;
    return row ? decodeJson<CardAnalytics>(row.data, {} as CardAnalytics) : undefined;
  }

  upsertAnalytics(analytics: CardAnalytics): void {
    this.db
      .prepare("INSERT OR REPLACE INTO card_analytics (card_id, data, updated_at) VALUES (?, ?, ?)")
      .run(analytics.cardId, encodeJson(analytics), nowMs());
  }

  replaceAnalytics(items: CardAnalytics[]): void {
    const tx = this.db.transaction((analytics: CardAnalytics[]) => {
      this.db.prepare("DELETE FROM card_analytics").run();
      for (const item of analytics) this.upsertAnalytics(item);
    });
    tx(items);
  }
}

function rowToTaskRun(row: SqliteRow<unknown>): KanbanTaskRun {
  return {
    id: String(row.id),
    taskId: String(row.task_id),
    boardId: String(row.board_id),
    orchestratorTaskId: String(row.orchestrator_task_id),
    status: row.status as KanbanTaskRun["status"],
    worktreePath: typeof row.worktree_path === "string" ? row.worktree_path : undefined,
    branch: typeof row.branch === "string" ? row.branch : undefined,
    baseRef: typeof row.base_ref === "string" ? row.base_ref : undefined,
    agentType: typeof row.agent_type === "string" ? row.agent_type : undefined,
    model: typeof row.model === "string" ? row.model : undefined,
    reasoningLevel: typeof row.reasoning_level === "string" ? row.reasoning_level : undefined,
    mode: row.mode === "plan" || row.mode === "instant" ? row.mode : undefined,
    summary: typeof row.summary === "string" ? row.summary : undefined,
    createdAt: Number(row.created_at),
    startedAt: row.started_at == null ? undefined : Number(row.started_at),
    completedAt: row.completed_at == null ? undefined : Number(row.completed_at),
  };
}

function stripTaskForStorage(task: KanbanCard): KanbanCard {
  const persisted = { ...task };
  delete persisted.latestRun;
  delete persisted.runs;
  delete persisted.activity;
  return persisted;
}

// ── Store Classes ────────────────────────────────────────────────

/**
 * Manages board configurations keyed by repoPath.
 * Provides default columns (Backlog/Ready/In Progress/In Review/Done) for new boards.
 */
export class BoardConfigStore {
  private db: KanbanDatabase;

  constructor(filePath?: string) {
    this.db = new KanbanDatabase(
      filePath ? sqlitePathForLegacyFile(filePath) : kanbanDbPath(),
      filePath ? { boards: filePath } : defaultLegacyImportPaths(),
    );
  }

  async load(): Promise<PersistedBoardData> {
    return { version: 1, boards: this.db.getBoards() };
  }

  async save(data: PersistedBoardData): Promise<void> {
    this.db.saveBoards(data.boards);
  }

  async getBoard(repoPath: string): Promise<BoardConfig | undefined> {
    return this.db.getBoards().find((b) => b.repoPath === repoPath);
  }

  async getBoardById(boardId: string): Promise<BoardConfig | undefined> {
    return this.db.getBoardById(boardId);
  }

  async saveBoard(board: BoardConfig): Promise<void> {
    this.db.upsertBoard(board);
  }

  async deleteBoard(boardId: string): Promise<void> {
    const byId = this.db.getBoardById(boardId);
    if (byId) {
      this.db.deleteBoard(boardId);
      return;
    }
    const byRepo = this.db.getBoards().find((board) => board.repoPath === boardId);
    if (byRepo) this.db.deleteBoard(byRepo.id);
  }

  /** Returns the default column set for new boards. */
  static defaultColumns(): KanbanColumn[] {
    return DEFAULT_COLUMNS.map((col) => ({ ...col }));
  }
}

/**
 * Manages automation rule definitions.
 * Handles missing file gracefully by returning an empty rule set.
 */
export class RuleStore {
  private db: KanbanDatabase;

  constructor(filePath?: string) {
    this.db = new KanbanDatabase(
      filePath ? sqlitePathForLegacyFile(filePath) : kanbanDbPath(),
      filePath ? { rules: filePath } : defaultLegacyImportPaths(),
    );
  }

  async load(): Promise<PersistedRuleData> {
    return { version: 1, rules: this.db.getRules() };
  }

  async save(data: PersistedRuleData): Promise<void> {
    this.db.replaceRules(data.rules);
  }

  async getRules(): Promise<AutomationRule[]> {
    return this.db.getRules();
  }

  async saveRule(rule: AutomationRule): Promise<void> {
    this.db.upsertRule(rule);
  }

  async deleteRule(ruleId: string): Promise<void> {
    this.db.deleteRule(ruleId);
  }
}

/**
 * Manages per-card analytics data.
 * Handles missing file gracefully by returning an empty analytics set.
 */
export class AnalyticsStore {
  private db: KanbanDatabase;

  constructor(filePath?: string) {
    this.db = new KanbanDatabase(
      filePath ? sqlitePathForLegacyFile(filePath) : kanbanDbPath(),
      filePath ? { analytics: filePath } : defaultLegacyImportPaths(),
    );
  }

  async load(): Promise<PersistedAnalyticsData> {
    return { version: 1, analytics: this.db.getAnalytics() };
  }

  async save(data: PersistedAnalyticsData): Promise<void> {
    this.db.replaceAnalytics(data.analytics);
  }

  async getCardAnalytics(cardId: string): Promise<CardAnalytics | undefined> {
    return this.db.getCardAnalytics(cardId);
  }

  async saveCardAnalytics(analytics: CardAnalytics): Promise<void> {
    this.db.upsertAnalytics(analytics);
  }
}

/**
 * Manages runtime board state (card positions).
 * Handles missing file gracefully by returning an empty card set.
 */
export class BoardStateStore {
  private db: KanbanDatabase;

  constructor(filePath?: string) {
    this.db = new KanbanDatabase(
      filePath ? sqlitePathForLegacyFile(filePath) : kanbanDbPath(),
      filePath ? { state: filePath } : defaultLegacyImportPaths(),
    );
  }

  async load(): Promise<PersistedBoardState> {
    return { version: 1, cards: this.db.getTasks() };
  }

  async save(data: PersistedBoardState): Promise<void> {
    this.db.replaceAllTasks(data.cards);
  }

  async getCards(): Promise<KanbanCard[]> {
    return this.db.getTasks();
  }

  async saveCards(cards: KanbanCard[]): Promise<void> {
    this.db.replaceAllTasks(cards);
  }

  async getTask(taskId: string): Promise<KanbanCard | undefined> {
    return this.db.getTask(taskId);
  }

  async getTaskByOrchestratorTask(orchestratorTaskId: string): Promise<KanbanCard | undefined> {
    return this.db.getTaskByOrchestratorTask(orchestratorTaskId);
  }

  async saveTask(task: KanbanCard): Promise<void> {
    this.db.upsertTask(task);
  }

  async deleteTask(taskId: string): Promise<void> {
    this.db.deleteTask(taskId);
  }

  async getTaskRuns(taskId: string): Promise<KanbanTaskRun[]> {
    return this.db.getRuns(taskId);
  }

  async saveTaskRun(run: KanbanTaskRun): Promise<KanbanTaskRun> {
    return this.db.addTaskRun(run);
  }

  async updateTaskRun(runId: string, updates: Partial<KanbanTaskRun>): Promise<KanbanTaskRun | null> {
    return this.db.updateTaskRun(runId, updates);
  }

  async getRunByOrchestratorTask(orchestratorTaskId: string): Promise<KanbanTaskRun | undefined> {
    return this.db.getRunByOrchestratorTask(orchestratorTaskId);
  }

  async getTaskActivity(taskId: string): Promise<KanbanTaskActivity[]> {
    return this.db.getActivity(taskId);
  }

  async addTaskActivity(
    entry: Omit<KanbanTaskActivity, "id" | "createdAt"> & { id?: string; createdAt?: number },
  ): Promise<KanbanTaskActivity> {
    return this.db.addActivity(entry);
  }
}

// ── Helpers ──────────────────────────────────────────────────────

function isNodeError(err: unknown): err is NodeJS.ErrnoException {
  return err instanceof Error && "code" in err;
}
