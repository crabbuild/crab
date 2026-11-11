import { afterEach, describe, expect, it } from "vitest";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import Sqlite from "better-sqlite3";
import { ChatDatabase, type ChatMessageRow } from "../electron/orchestrator-chat-db";

const databases: Array<{ db: ChatDatabase; directory: string }> = [];
afterEach(() => {
  for (const { db, directory } of databases.splice(0)) { db.close(); rmSync(directory, { recursive: true, force: true }); }
});
function database(): ChatDatabase {
  const directory = mkdtempSync(path.join(tmpdir(), "crab-chat-db-"));
  const db = new ChatDatabase(path.join(directory, "chats.sqlite"));
  databases.push({ db, directory });
  return db;
}
function message(id: string, content: string, taskId = "task"): ChatMessageRow {
  return { id, task_id: taskId, role: "agent", kind: "text", content, events: null, timestamp: 1 };
}

describe("durable chat rows", () => {
  it("upserts changed messages without replacing prior history or another task", () => {
    const db = database();
    db.updateMessages("task", [message("first", "First"), message("second", "Partial")]);
    db.updateMessages("other-task", [message("other", "Other", "other-task")]);
    db.updateMessages("task", [{ ...message("second", "Completed"), metadata: JSON.stringify({ itemId: "block", sequence: 2 }) }]);
    expect({ own: db.loadMessages("task").map(row => [row.id, row.content, row.metadata]), other: db.loadMessages("other-task").map(row => row.content) }).toEqual({
      own: [["first", "First", null], ["second", "Completed", JSON.stringify({ itemId: "block", sequence: 2 })]], other: ["Other"],
    });
  });

  it("updates full-text search when a streamed snapshot replaces earlier text", () => {
    const db = database();
    db.insertMessage(message("block", "obsoleteword"));
    db.insertMessage(message("block", "completedword"));
    expect({ before: db.searchInTask("task", "obsoleteword").map(row => row.id), after: db.searchInTask("task", "completedword").map(row => row.id) }).toEqual({ before: [], after: ["block"] });
  });

  it("adds transcript metadata to an existing database while preserving its history and search index", () => {
    const original = database();
    original.insertMessage(message("existing", "Earlier history"));
    const fixture = databases.pop();
    if (!fixture) throw new Error("Database fixture was not registered");
    original.close();
    const location = path.join(fixture.directory, "chats.sqlite");
    const old = new Sqlite(location);
    old.exec("ALTER TABLE messages DROP COLUMN metadata");
    old.close();
    const upgraded = new ChatDatabase(location);
    databases.push({ db: upgraded, directory: fixture.directory });
    expect({ rows: upgraded.loadMessages("task").map(row => [row.content, row.metadata]), search: upgraded.searchInTask("task", "Earlier").map(row => row.id) }).toEqual({ rows: [["Earlier history", null]], search: ["existing"] });
  });
});
