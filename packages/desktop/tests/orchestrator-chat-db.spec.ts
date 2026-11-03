import { afterEach, describe, expect, it, vi } from "vitest";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { ChatDatabase, type ChatMessageRow } from "../electron/orchestrator-chat-db";

vi.mock("better-sqlite3", () => {
  interface Row {
    id: string;
    task_id: string;
    role: "user" | "agent";
    kind: string | null;
    content: string;
    events: string | null;
    timestamp: number;
  }

  class FakeDatabase {
    private rows: Row[] = [];

    pragma(): void {}
    exec(): void {}

    prepare(sql: string) {
      if (sql.includes("SELECT name FROM sqlite_master")) {
        return { get: () => ({ name: "messages_fts" }) };
      }
      if (sql.includes("INSERT OR REPLACE INTO messages")) {
        return {
          run: (
            id: string,
            taskId: string,
            role: "user" | "agent",
            kind: string | null,
            content: string,
            events: string | null,
            timestamp: number,
          ) => {
            this.rows = this.rows.filter((row) => row.id !== id);
            this.rows.push({ id, task_id: taskId, role, kind, content, events, timestamp });
          },
        };
      }
      if (sql.includes("DELETE FROM messages WHERE task_id = ?")) {
        return {
          run: (taskId: string) => {
            this.rows = this.rows.filter((row) => row.task_id !== taskId);
          },
        };
      }
      if (sql.includes("SELECT * FROM messages WHERE task_id = ? ORDER BY timestamp ASC")) {
        return {
          all: (taskId: string) =>
            this.rows
              .filter((row) => row.task_id === taskId)
              .sort((a, b) => a.timestamp - b.timestamp),
        };
      }
      throw new Error(`Unexpected SQL in fake better-sqlite3: ${sql}`);
    }

    transaction<T extends unknown[]>(fn: (...args: T) => void) {
      return (...args: T) => fn(...args);
    }
  }

  return { default: FakeDatabase };
});

let tempDirs: string[] = [];

function createDb(): ChatDatabase {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "crab-chat-db-test-"));
  tempDirs.push(dir);
  return new ChatDatabase(path.join(dir, "agent-chats.sqlite"));
}

function message(overrides: Partial<ChatMessageRow>): ChatMessageRow {
  return {
    id: "msg",
    task_id: "task-1",
    role: "agent",
    kind: "text",
    content: "hello",
    events: null,
    timestamp: 1,
    ...overrides,
  };
}

afterEach(() => {
  for (const dir of tempDirs) {
    fs.rmSync(dir, { recursive: true, force: true });
  }
  tempDirs = [];
});

describe("ChatDatabase", () => {
  it("replaces a task transcript when saving renderer messages", () => {
    const db = createDb();

    db.insertMessage(message({
      id: "task-1-main-fallback",
      kind: "done",
      content: "final answer",
      timestamp: 1,
    }));
    db.insertMessage(message({
      id: "other-task-message",
      task_id: "task-2",
      content: "keep me",
      timestamp: 1,
    }));

    db.saveMessages("task-1", [
      message({
        id: "task-1-renderer-final",
        kind: "done",
        content: "final answer",
        timestamp: 2,
      }),
    ]);

    expect(db.loadMessages("task-1").map((row) => row.id)).toEqual([
      "task-1-renderer-final",
    ]);
    expect(db.loadMessages("task-2").map((row) => row.id)).toEqual([
      "other-task-message",
    ]);
  });
});
