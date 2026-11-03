/**
 * Event Persistence for the Crab Desktop Orchestrator.
 *
 * Persists activity events to JSONL files for archived tasks so users
 * can review what the agent did after restart.
 *
 * Storage: ~/.crab/desktop/agent-events/<task-id>.jsonl
 */

import * as fs from "node:fs";
import * as path from "node:path";
import * as os from "node:os";

const EVENTS_DIR = path.join(os.homedir(), ".crab", "desktop", "agent-events");

/**
 * Persists task events to a JSONL file.
 */
export async function persistEvents(taskId: string, events: any[]): Promise<void> {
  try {
    fs.mkdirSync(EVENTS_DIR, { recursive: true });
    const filePath = path.join(EVENTS_DIR, `${taskId}.jsonl`);
    const lines = events.map((e) => JSON.stringify(e)).join("\n");
    fs.writeFileSync(filePath, lines + "\n", "utf-8");
  } catch {
    // Best-effort persistence
  }
}

/**
 * Loads persisted events for a task.
 */
export function loadEvents(taskId: string): any[] {
  try {
    const filePath = path.join(EVENTS_DIR, `${taskId}.jsonl`);
    if (!fs.existsSync(filePath)) return [];
    const content = fs.readFileSync(filePath, "utf-8");
    return content
      .trim()
      .split("\n")
      .filter((line) => line.trim())
      .map((line) => JSON.parse(line));
  } catch {
    return [];
  }
}

/**
 * Deletes persisted events for a task.
 */
export function deleteEvents(taskId: string): void {
  try {
    const filePath = path.join(EVENTS_DIR, `${taskId}.jsonl`);
    if (fs.existsSync(filePath)) {
      fs.unlinkSync(filePath);
    }
  } catch {
    // Best-effort
  }
}

/**
 * Appends a single event to the JSONL file (for real-time persistence).
 */
export function appendEvent(taskId: string, event: any): void {
  try {
    fs.mkdirSync(EVENTS_DIR, { recursive: true });
    const filePath = path.join(EVENTS_DIR, `${taskId}.jsonl`);
    fs.appendFileSync(filePath, JSON.stringify(event) + "\n", "utf-8");
  } catch {
    // Best-effort
  }
}
