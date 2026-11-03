/**
 * SQLite-backed task metadata persistence for the orchestrator.
 *
 * The events stream remains ephemeral/in its own persistence path; this database
 * stores restart-critical task metadata and sidebar organization state.
 */

import Database from "better-sqlite3";
import * as fs from "node:fs";
import * as path from "node:path";
import * as os from "node:os";

import type { PersistedTask, TaskStatus } from "./orchestrator";

const DB_DIR = path.join(os.homedir(), ".crab", "desktop");
const DB_PATH = path.join(DB_DIR, "agent-tasks.sqlite");

type TaskRow = {
  id: string;
  title: string | null;
  prompt: string;
  agent_type: string;
  status: TaskStatus;
  branch: string;
  worktree_path: string;
  worktree_mode: PersistedTask["worktreeMode"] | null;
  base_ref: string;
  base_branch: string | null;
  repo_path: string;
  created_at: number;
  started_at: number | null;
  completed_at: number | null;
  exit_code: number | null;
  pr_url: string | null;
  pr_number: number | null;
  is_remote: number;
  remote_profile_id: string | null;
  codex_thread_id: string | null;
  ui_state_json: string | null;
  kanban_card_id: string | null;
  workflow_run_id: string | null;
  workflow_node_run_id: string | null;
  plan_first: number;
  model: string | null;
  reasoning_level: string | null;
  pinned_at: number | null;
  parent_task_id: string | null;
  root_task_id: string | null;
  spawned_by_agent_id: string | null;
  spawned_role: string | null;
  last_activity_at: number | null;
  sidebar_label: string | null;
  sidebar_group: string | null;
};

type AgentRow = {
  id: string;
  task_id: string;
  type: string;
  role: string | null;
  status: string | null;
  child_task_id: string | null;
};

type SqliteStatement = {
  run: (...args: any[]) => unknown;
  get: (...args: any[]) => unknown;
  all: (...args: any[]) => unknown[];
};

type SqliteDatabase = {
  prepare: (sql: string) => SqliteStatement;
  exec: (sql: string) => void;
  transaction: <T extends (...args: any[]) => unknown>(fn: T) => T;
  close: () => void;
};

function encodeJson(value: unknown): string | null {
  if (value == null) return null;
  return JSON.stringify(value);
}

function decodeJson<T>(value: string | null | undefined, fallback: T): T {
  if (!value) return fallback;
  try {
    return JSON.parse(value) as T;
  } catch {
    return fallback;
  }
}

export function sqlitePathForPersistPath(persistPath?: string): string {
  if (!persistPath) return DB_PATH;
  if (persistPath.endsWith(".json")) {
    return `${persistPath.slice(0, -".json".length)}.sqlite`;
  }
  if (persistPath.endsWith(".sqlite") || persistPath.endsWith(".db") || persistPath.endsWith(".sqlite3")) {
    return persistPath;
  }
  return `${persistPath}.sqlite`;
}

function createNodeSqliteDatabase(dbPath: string): SqliteDatabase {
  const { DatabaseSync } = require("node:sqlite") as {
    DatabaseSync: new (path: string) => {
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
    transaction: <T extends (...args: any[]) => unknown>(fn: T): T => {
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

export class OrchestratorTaskDatabase {
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

  countTasks(): number {
    const row = this.db.prepare("SELECT COUNT(*) AS count FROM tasks").get() as { count: number } | undefined;
    return row?.count ?? 0;
  }

  loadTasks(): PersistedTask[] {
    const rows = this.db.prepare("SELECT * FROM tasks ORDER BY created_at ASC").all() as TaskRow[];
    const agentsByTask = new Map<string, AgentRow[]>();
    for (const row of this.db.prepare("SELECT * FROM task_agents ORDER BY task_id, rowid ASC").all() as AgentRow[]) {
      const list = agentsByTask.get(row.task_id) ?? [];
      list.push(row);
      agentsByTask.set(row.task_id, list);
    }

    const feedbackByTask = new Map<string, string[]>();
    for (const row of this.db.prepare("SELECT task_id, text FROM task_feedback ORDER BY task_id, position ASC").all() as Array<{ task_id: string; text: string }>) {
      const list = feedbackByTask.get(row.task_id) ?? [];
      list.push(row.text);
      feedbackByTask.set(row.task_id, list);
    }

    const todosByTask = new Map<string, PersistedTask["todos"]>();
    for (const row of this.db.prepare("SELECT * FROM task_todos ORDER BY task_id, position ASC").all() as Array<{ task_id: string; id: string; text: string; done: number; source: "user" | "agent" | "comment" }>) {
      const list = todosByTask.get(row.task_id) ?? [];
      list.push({ id: row.id, text: row.text, done: row.done === 1, source: row.source });
      todosByTask.set(row.task_id, list);
    }

    return rows.map((row) => ({
      id: row.id,
      title: row.title ?? undefined,
      prompt: row.prompt,
      agentType: row.agent_type as PersistedTask["agentType"],
      status: row.status,
      branch: row.branch,
      worktreePath: row.worktree_path,
      worktreeMode: row.worktree_mode ?? undefined,
      baseRef: row.base_ref,
      baseBranch: row.base_branch ?? undefined,
      repoPath: row.repo_path,
      createdAt: row.created_at,
      startedAt: row.started_at ?? undefined,
      completedAt: row.completed_at ?? undefined,
      exitCode: row.exit_code ?? undefined,
      feedback: feedbackByTask.get(row.id) ?? [],
      todos: todosByTask.get(row.id) ?? [],
      prUrl: row.pr_url ?? undefined,
      prNumber: row.pr_number ?? undefined,
      agents: (agentsByTask.get(row.id) ?? []).map((agent) => ({
        id: agent.id,
        type: agent.type,
        role: agent.role ?? undefined,
        status: agent.status ?? undefined,
        childTaskId: agent.child_task_id ?? undefined,
      })),
      isRemote: row.is_remote === 1 || undefined,
      remoteProfileId: row.remote_profile_id ?? undefined,
      codexThreadId: row.codex_thread_id ?? undefined,
      uiState: decodeJson<PersistedTask["uiState"]>(row.ui_state_json, undefined),
      kanbanCardId: row.kanban_card_id ?? undefined,
      workflowRunId: row.workflow_run_id ?? undefined,
      workflowNodeRunId: row.workflow_node_run_id ?? undefined,
      planFirst: row.plan_first === 1 || undefined,
      model: row.model ?? undefined,
      reasoningLevel: row.reasoning_level ?? undefined,
      pinnedAt: row.pinned_at ?? undefined,
      parentTaskId: row.parent_task_id ?? undefined,
      rootTaskId: row.root_task_id ?? undefined,
      spawnedByAgentId: row.spawned_by_agent_id ?? undefined,
      spawnedRole: row.spawned_role ?? undefined,
      lastActivityAt: row.last_activity_at ?? undefined,
      sidebarLabel: row.sidebar_label ?? undefined,
      sidebarGroup: row.sidebar_group ?? undefined,
    }));
  }

  replaceTasks(tasks: PersistedTask[]): void {
    const upsertTask = this.db.prepare(`
      INSERT INTO tasks (
        id, title, prompt, agent_type, status, branch, worktree_path,
        worktree_mode, base_ref, base_branch, repo_path, created_at,
        started_at, completed_at, exit_code, pr_url, pr_number, is_remote,
        remote_profile_id, codex_thread_id, ui_state_json, kanban_card_id,
        workflow_run_id, workflow_node_run_id, plan_first, model,
        reasoning_level, pinned_at, parent_task_id, root_task_id,
        spawned_by_agent_id, spawned_role, last_activity_at, sidebar_label,
        sidebar_group, updated_at
      ) VALUES (
        @id, @title, @prompt, @agent_type, @status, @branch,
        @worktree_path, @worktree_mode, @base_ref, @base_branch, @repo_path,
        @created_at, @started_at, @completed_at, @exit_code, @pr_url,
        @pr_number, @is_remote, @remote_profile_id, @codex_thread_id,
        @ui_state_json, @kanban_card_id, @workflow_run_id,
        @workflow_node_run_id, @plan_first, @model, @reasoning_level,
        @pinned_at, @parent_task_id, @root_task_id, @spawned_by_agent_id,
        @spawned_role, @last_activity_at, @sidebar_label, @sidebar_group,
        @updated_at
      )
      ON CONFLICT(id) DO UPDATE SET
        title = excluded.title,
        prompt = excluded.prompt,
        agent_type = excluded.agent_type,
        status = excluded.status,
        branch = excluded.branch,
        worktree_path = excluded.worktree_path,
        worktree_mode = excluded.worktree_mode,
        base_ref = excluded.base_ref,
        base_branch = excluded.base_branch,
        repo_path = excluded.repo_path,
        created_at = excluded.created_at,
        started_at = excluded.started_at,
        completed_at = excluded.completed_at,
        exit_code = excluded.exit_code,
        pr_url = excluded.pr_url,
        pr_number = excluded.pr_number,
        is_remote = excluded.is_remote,
        remote_profile_id = excluded.remote_profile_id,
        codex_thread_id = excluded.codex_thread_id,
        ui_state_json = excluded.ui_state_json,
        kanban_card_id = excluded.kanban_card_id,
        workflow_run_id = excluded.workflow_run_id,
        workflow_node_run_id = excluded.workflow_node_run_id,
        plan_first = excluded.plan_first,
        model = excluded.model,
        reasoning_level = excluded.reasoning_level,
        pinned_at = excluded.pinned_at,
        parent_task_id = excluded.parent_task_id,
        root_task_id = excluded.root_task_id,
        spawned_by_agent_id = excluded.spawned_by_agent_id,
        spawned_role = excluded.spawned_role,
        last_activity_at = excluded.last_activity_at,
        sidebar_label = excluded.sidebar_label,
        sidebar_group = excluded.sidebar_group,
        updated_at = excluded.updated_at
    `);
    const insertAgent = this.db.prepare(`
      INSERT INTO task_agents (task_id, id, type, role, status, child_task_id)
      VALUES (?, ?, ?, ?, ?, ?)
    `);
    const insertFeedback = this.db.prepare(`
      INSERT INTO task_feedback (task_id, position, text)
      VALUES (?, ?, ?)
    `);
    const insertTodo = this.db.prepare(`
      INSERT INTO task_todos (task_id, id, position, text, done, source)
      VALUES (?, ?, ?, ?, ?, ?)
    `);

    const tx = this.db.transaction((nextTasks: PersistedTask[]) => {
      const ids = new Set(nextTasks.map((task) => task.id));
      for (const row of this.db.prepare("SELECT id FROM tasks").all() as Array<{ id: string }>) {
        if (!ids.has(row.id)) {
          this.db.prepare("DELETE FROM tasks WHERE id = ?").run(row.id);
        }
      }

      for (const task of nextTasks) {
        upsertTask.run({
          id: task.id,
          title: task.title ?? null,
          prompt: task.prompt,
          agent_type: task.agentType,
          status: task.status,
          branch: task.branch,
          worktree_path: task.worktreePath,
          worktree_mode: task.worktreeMode ?? null,
          base_ref: task.baseRef,
          base_branch: task.baseBranch ?? null,
          repo_path: task.repoPath,
          created_at: task.createdAt,
          started_at: task.startedAt ?? null,
          completed_at: task.completedAt ?? null,
          exit_code: task.exitCode ?? null,
          pr_url: task.prUrl ?? null,
          pr_number: task.prNumber ?? null,
          is_remote: task.isRemote ? 1 : 0,
          remote_profile_id: task.remoteProfileId ?? null,
          codex_thread_id: task.codexThreadId ?? null,
          ui_state_json: encodeJson(task.uiState),
          kanban_card_id: task.kanbanCardId ?? null,
          workflow_run_id: task.workflowRunId ?? null,
          workflow_node_run_id: task.workflowNodeRunId ?? null,
          plan_first: task.planFirst ? 1 : 0,
          model: task.model ?? null,
          reasoning_level: task.reasoningLevel ?? null,
          pinned_at: task.pinnedAt ?? null,
          parent_task_id: task.parentTaskId ?? null,
          root_task_id: task.rootTaskId ?? null,
          spawned_by_agent_id: task.spawnedByAgentId ?? null,
          spawned_role: task.spawnedRole ?? null,
          last_activity_at: task.lastActivityAt ?? null,
          sidebar_label: task.sidebarLabel ?? null,
          sidebar_group: task.sidebarGroup ?? null,
          updated_at: Date.now(),
        });

        this.db.prepare("DELETE FROM task_feedback WHERE task_id = ?").run(task.id);
        this.db.prepare("DELETE FROM task_todos WHERE task_id = ?").run(task.id);
        this.db.prepare("DELETE FROM task_agents WHERE task_id = ?").run(task.id);
      }

      for (const task of nextTasks) {
        for (const agent of task.agents ?? []) {
          insertAgent.run(task.id, agent.id, agent.type, agent.role ?? null, agent.status ?? null, agent.childTaskId ?? null);
        }
        for (const [index, text] of (task.feedback ?? []).entries()) {
          insertFeedback.run(task.id, index, text);
        }
        for (const [index, todo] of (task.todos ?? []).entries()) {
          insertTodo.run(task.id, todo.id, index, todo.text, todo.done ? 1 : 0, todo.source);
        }
      }
    });

    tx(tasks);
  }

  deleteTask(taskId: string): void {
    this.db.prepare("DELETE FROM tasks WHERE id = ?").run(taskId);
  }

  private migrate(): void {
    this.db.exec(`
      CREATE TABLE IF NOT EXISTS tasks (
        id TEXT PRIMARY KEY,
        title TEXT,
        prompt TEXT NOT NULL,
        agent_type TEXT NOT NULL,
        status TEXT NOT NULL,
        branch TEXT NOT NULL,
        worktree_path TEXT NOT NULL,
        worktree_mode TEXT,
        base_ref TEXT NOT NULL,
        base_branch TEXT,
        repo_path TEXT NOT NULL,
        created_at INTEGER NOT NULL,
        started_at INTEGER,
        completed_at INTEGER,
        exit_code INTEGER,
        pr_url TEXT,
        pr_number INTEGER,
        is_remote INTEGER NOT NULL DEFAULT 0,
        remote_profile_id TEXT,
        codex_thread_id TEXT,
        ui_state_json TEXT,
        kanban_card_id TEXT,
        workflow_run_id TEXT,
        workflow_node_run_id TEXT,
        plan_first INTEGER NOT NULL DEFAULT 0,
        model TEXT,
        reasoning_level TEXT,
        pinned_at INTEGER,
        parent_task_id TEXT REFERENCES tasks(id) ON DELETE SET NULL,
        root_task_id TEXT,
        spawned_by_agent_id TEXT,
        spawned_role TEXT,
        last_activity_at INTEGER,
        sidebar_label TEXT,
        sidebar_group TEXT,
        updated_at INTEGER NOT NULL DEFAULT 0
      );

      CREATE INDEX IF NOT EXISTS idx_tasks_repo_status
        ON tasks(repo_path, status, created_at DESC);
      CREATE INDEX IF NOT EXISTS idx_tasks_parent
        ON tasks(parent_task_id, created_at ASC);
      CREATE INDEX IF NOT EXISTS idx_tasks_pinned
        ON tasks(pinned_at DESC);

      CREATE TABLE IF NOT EXISTS task_agents (
        task_id TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
        id TEXT NOT NULL,
        type TEXT NOT NULL,
        role TEXT,
        status TEXT,
        child_task_id TEXT,
        PRIMARY KEY (task_id, id)
      );

      CREATE TABLE IF NOT EXISTS task_feedback (
        task_id TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
        position INTEGER NOT NULL,
        text TEXT NOT NULL,
        PRIMARY KEY (task_id, position)
      );

      CREATE TABLE IF NOT EXISTS task_todos (
        task_id TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
        id TEXT NOT NULL,
        position INTEGER NOT NULL,
        text TEXT NOT NULL,
        done INTEGER NOT NULL DEFAULT 0,
        source TEXT NOT NULL,
        PRIMARY KEY (task_id, id)
      );
    `);

    this.ensureColumn("tasks", "title", "TEXT");
    this.ensureColumn("tasks", "base_branch", "TEXT");
    this.ensureColumn("tasks", "worktree_mode", "TEXT");
    this.ensureColumn("tasks", "started_at", "INTEGER");
    this.ensureColumn("tasks", "completed_at", "INTEGER");
    this.ensureColumn("tasks", "exit_code", "INTEGER");
    this.ensureColumn("tasks", "pr_url", "TEXT");
    this.ensureColumn("tasks", "pr_number", "INTEGER");
    this.ensureColumn("tasks", "is_remote", "INTEGER NOT NULL DEFAULT 0");
    this.ensureColumn("tasks", "remote_profile_id", "TEXT");
    this.ensureColumn("tasks", "codex_thread_id", "TEXT");
    this.ensureColumn("tasks", "ui_state_json", "TEXT");
    this.ensureColumn("tasks", "kanban_card_id", "TEXT");
    this.ensureColumn("tasks", "workflow_run_id", "TEXT");
    this.ensureColumn("tasks", "workflow_node_run_id", "TEXT");
    this.ensureColumn("tasks", "plan_first", "INTEGER NOT NULL DEFAULT 0");
    this.ensureColumn("tasks", "model", "TEXT");
    this.ensureColumn("tasks", "reasoning_level", "TEXT");
    this.ensureColumn("tasks", "pinned_at", "INTEGER");
    this.ensureColumn("tasks", "parent_task_id", "TEXT");
    this.ensureColumn("tasks", "root_task_id", "TEXT");
    this.ensureColumn("tasks", "spawned_by_agent_id", "TEXT");
    this.ensureColumn("tasks", "spawned_role", "TEXT");
    this.ensureColumn("tasks", "last_activity_at", "INTEGER");
    this.ensureColumn("tasks", "sidebar_label", "TEXT");
    this.ensureColumn("tasks", "sidebar_group", "TEXT");
    this.ensureColumn("tasks", "updated_at", "INTEGER NOT NULL DEFAULT 0");
    this.ensureColumn("task_agents", "role", "TEXT");
    this.ensureColumn("task_agents", "status", "TEXT");
    this.ensureColumn("task_agents", "child_task_id", "TEXT");
  }

  private ensureColumn(table: "tasks" | "task_agents", column: string, definition: string): void {
    const rows = this.db.prepare(`PRAGMA table_info(${table})`).all() as Array<{ name: string }>;
    if (rows.some((row) => row.name === column)) return;
    this.db.exec(`ALTER TABLE ${table} ADD COLUMN ${column} ${definition}`);
  }
}
