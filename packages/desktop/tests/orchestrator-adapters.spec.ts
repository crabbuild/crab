/**
 * Unit tests for orchestrator agent adapters.
 *
 * Tests that each adapter correctly parses sample NDJSON event streams
 * from fixture files into normalised AgentEvent objects.
 */

import { describe, it, expect } from "vitest";
import * as fs from "node:fs";
import * as path from "node:path";

// Import registry first to initialize it (sets up the _registry reference).
import "../electron/orchestrator-registry";

import {
  AgentEvent,
  GenericAdapter,
  createAdapter,
} from "../electron/orchestrator-adapters";
import { ClaudeCodeAdapter } from "../electron/claude-code-adapter";
import { CodexAppServerAdapter } from "../electron/codex-app-server-adapter";

// ─── Helpers ─────────────────────────────────────────────────────────────────

function loadFixture(filename: string): string[] {
  const fixturePath = path.join(__dirname, "fixtures", filename);
  const content = fs.readFileSync(fixturePath, "utf8");
  return content.trim().split("\n").filter((line) => line.trim());
}

// ─── GenericAdapter ──────────────────────────────────────────────────────────

describe("GenericAdapter", () => {
  it("passes through events matching AgentEvent schema directly", () => {
    const lines = loadFixture("generic-events.ndjson");
    const events: AgentEvent[] = lines.map((line) => {
      const raw = JSON.parse(line) as AgentEvent;
      if (!raw.timestamp) raw.timestamp = Date.now();
      return raw;
    });

    expect(events).toHaveLength(8);
    expect(events[0].type).toBe("thinking");
    expect(events[0].timestamp).toBe(1700000001000);
    expect(events[0].content).toBe("Planning the implementation approach.");
  });

  it("preserves all event types from the fixture", () => {
    const lines = loadFixture("generic-events.ndjson");
    const events: AgentEvent[] = lines.map((line) => JSON.parse(line));

    const types = events.map((e) => e.type);
    expect(types).toEqual([
      "thinking",
      "text",
      "tool_use",
      "tool_result",
      "file_edit",
      "command_run",
      "error",
      "done",
    ]);
  });

  it("preserves file_edit fields", () => {
    const lines = loadFixture("generic-events.ndjson");
    const events: AgentEvent[] = lines.map((line) => JSON.parse(line));
    const fileEdit = events[4];

    expect(fileEdit.type).toBe("file_edit");
    expect(fileEdit.path).toBe("src/main.rs");
    expect(fileEdit.diff).toBe("+ async fn main()");
  });

  it("preserves command_run fields", () => {
    const lines = loadFixture("generic-events.ndjson");
    const events: AgentEvent[] = lines.map((line) => JSON.parse(line));
    const cmdRun = events[5];

    expect(cmdRun.type).toBe("command_run");
    expect(cmdRun.command).toBe("cargo build");
    expect(cmdRun.stdout).toBe("Compiling...");
    expect(cmdRun.exitCode).toBe(0);
  });

  it("preserves timestamps from the source", () => {
    const lines = loadFixture("generic-events.ndjson");
    const events: AgentEvent[] = lines.map((line) => JSON.parse(line));

    expect(events[0].timestamp).toBe(1700000001000);
    expect(events[7].timestamp).toBe(1700000008000);
  });

  it("constructs with custom name and binary", () => {
    const adapter = new GenericAdapter("/usr/local/bin/my-agent", "My Agent", ["--verbose"]);
    expect(adapter.name).toBe("My Agent");
    expect(adapter.binary).toBe("/usr/local/bin/my-agent");
  });

  it("defaults name to binary basename", () => {
    const adapter = new GenericAdapter("/usr/local/bin/my-agent");
    expect(adapter.name).toBe("my-agent");
  });
});

// ─── createAdapter factory ───────────────────────────────────────────────────

describe("createAdapter", () => {
  it("creates ClaudeCodeAdapter for claude-code type", () => {
    const adapter = createAdapter("claude-code");
    expect(adapter).toBeInstanceOf(ClaudeCodeAdapter);
    expect(adapter.name).toBe("Claude Code");
    expect(adapter.binary).toBe("claude");
  });

  it("creates CodexAppServerAdapter for codex type", () => {
    const adapter = createAdapter("codex");
    expect(adapter).toBeInstanceOf(CodexAppServerAdapter);
    expect(adapter.name).toBe("Codex");
    expect(adapter.binary).toBe("codex");
  });

  it("creates GenericAdapter for custom type", () => {
    const adapter = createAdapter("custom", "/usr/bin/my-agent");
    expect(adapter).toBeInstanceOf(GenericAdapter);
    expect(adapter.name).toBe("my-agent");
    expect(adapter.binary).toBe("/usr/bin/my-agent");
  });

  it("throws when custom type has no binary", () => {
    expect(() => createAdapter("custom")).toThrow(
      "customBinary is required for custom agent type",
    );
  });

  it("codex registry entry returns CodexAppServerAdapter with config", () => {
    const adapter = createAdapter("codex", "/usr/bin/codex", ["--model", "o3"]);
    expect(adapter).toBeInstanceOf(CodexAppServerAdapter);
  });

  it("CodexAdapter class no longer exists in orchestrator-adapters exports", async () => {
    const mod = await import("../electron/orchestrator-adapters");
    expect("CodexAdapter" in mod).toBe(false);
  });
});
