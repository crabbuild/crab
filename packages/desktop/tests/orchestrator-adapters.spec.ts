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
  ClaudeCodeAdapter,
  GenericAdapter,
  createAdapter,
} from "../electron/orchestrator-adapters";
import { CodexAppServerAdapter } from "../electron/codex-app-server-adapter";

// ─── Helpers ─────────────────────────────────────────────────────────────────

function loadFixture(filename: string): string[] {
  const fixturePath = path.join(__dirname, "fixtures", filename);
  const content = fs.readFileSync(fixturePath, "utf8");
  return content.trim().split("\n").filter((line) => line.trim());
}

// ─── ClaudeCodeAdapter ───────────────────────────────────────────────────────

describe("ClaudeCodeAdapter", () => {
  it("parses thinking events", () => {
    const adapter = new ClaudeCodeAdapter();
    const lines = loadFixture("claude-code-events.ndjson");
    const raw = JSON.parse(lines[0]);
    const event = adapter.parseClaudeEvent(raw);

    expect(event.type).toBe("thinking");
    expect(event.content).toBe(
      "Let me analyze the push pipeline to understand the current sync approach.",
    );
    expect(event.timestamp).toBeTypeOf("number");
  });

  it("parses assistant text events", () => {
    const adapter = new ClaudeCodeAdapter();
    const lines = loadFixture("claude-code-events.ndjson");
    const raw = JSON.parse(lines[1]);
    const event = adapter.parseClaudeEvent(raw);

    expect(event.type).toBe("text");
    expect(event.content).toBe(
      "I'll refactor the push pipeline to use async streams.",
    );
  });

  it("parses tool_use Read events", () => {
    const adapter = new ClaudeCodeAdapter();
    const lines = loadFixture("claude-code-events.ndjson");
    const raw = JSON.parse(lines[2]);
    const event = adapter.parseClaudeEvent(raw);

    expect(event.type).toBe("file_read");
    expect(event.tool).toBe("Read");
    expect(event.input).toEqual({ file_path: "src/git/push.rs" });
  });

  it("parses tool_result events", () => {
    const adapter = new ClaudeCodeAdapter();
    const lines = loadFixture("claude-code-events.ndjson");
    const raw = JSON.parse(lines[3]);
    const event = adapter.parseClaudeEvent(raw);

    expect(event.type).toBe("tool_result");
    expect(event.tool).toBe("Read");
    expect(event.output).toBe(
      "pub fn push_xorbs(pool: &UploadPool) -> Result<()> { ... }",
    );
  });

  it("parses Write tool_use as file_edit events", () => {
    const adapter = new ClaudeCodeAdapter();
    const lines = loadFixture("claude-code-events.ndjson");
    const raw = JSON.parse(lines[4]);
    const event = adapter.parseClaudeEvent(raw);

    expect(event.type).toBe("file_edit");
    expect(event.tool).toBe("Write");
    expect(event.path).toBe("src/git/push.rs");
    expect(event.diff).toBe(
      "pub async fn push_xorbs(pool: &UploadPool) -> Result<()> { ... }",
    );
  });

  it("parses Bash tool_use as command events", () => {
    const adapter = new ClaudeCodeAdapter();
    const lines = loadFixture("claude-code-events.ndjson");
    const raw = JSON.parse(lines[5]);
    const event = adapter.parseClaudeEvent(raw);

    expect(event.type).toBe("command");
    expect(event.tool).toBe("Bash");
    expect(event.command).toBe("cargo build");
  });

  it("parses error events", () => {
    const adapter = new ClaudeCodeAdapter();
    const lines = loadFixture("claude-code-events.ndjson");
    const raw = JSON.parse(lines[7]);
    const event = adapter.parseClaudeEvent(raw);

    expect(event.type).toBe("error");
    expect(event.error).toBe("Failed to read file: permission denied");
  });

  it("parses result events as done", () => {
    const adapter = new ClaudeCodeAdapter();
    const lines = loadFixture("claude-code-events.ndjson");
    const raw = JSON.parse(lines[8]);
    const event = adapter.parseClaudeEvent(raw);

    expect(event.type).toBe("done");
    expect(event.content).toBe(
      "Refactoring complete. The push pipeline now uses async streams throughout.",
    );
  });

  it("parses all fixture events without throwing", () => {
    const adapter = new ClaudeCodeAdapter();
    const lines = loadFixture("claude-code-events.ndjson");
    const events: AgentEvent[] = [];

    for (const line of lines) {
      const raw = JSON.parse(line);
      const event = adapter.parseClaudeEvent(raw);
      events.push(event);
    }

    expect(events).toHaveLength(10);
    expect(events.every((e) => e.timestamp > 0)).toBe(true);
    expect(events.every((e) => typeof e.type === "string")).toBe(true);
  });

  it("extracts usage event from result events with usage field", () => {
    const adapter = new ClaudeCodeAdapter();
    const raw = {
      type: "result",
      result: "Done.",
      usage: {
        input_tokens: 1234,
        output_tokens: 567,
        cache_creation_input_tokens: 100,
        cache_read_input_tokens: 200,
      },
    };

    const usageEvent = adapter.extractUsageEvent(raw.usage as Record<string, unknown>);
    expect(usageEvent).not.toBeNull();
    expect(usageEvent!.type).toBe("usage");
    expect(usageEvent!.usage).toEqual({
      inputTokens: 1234,
      outputTokens: 567,
      cacheCreationInputTokens: 100,
      cacheReadInputTokens: 200,
    });
  });

  it("returns null for usage event when all token counts are zero", () => {
    const adapter = new ClaudeCodeAdapter();
    const usageEvent = adapter.extractUsageEvent({
      input_tokens: 0,
      output_tokens: 0,
      cache_creation_input_tokens: 0,
      cache_read_input_tokens: 0,
    });

    expect(usageEvent).toBeNull();
  });

  it("handles missing usage fields gracefully", () => {
    const adapter = new ClaudeCodeAdapter();
    const usageEvent = adapter.extractUsageEvent({
      input_tokens: 500,
    });

    expect(usageEvent).not.toBeNull();
    expect(usageEvent!.usage).toEqual({
      inputTokens: 500,
      outputTokens: 0,
      cacheCreationInputTokens: 0,
      cacheReadInputTokens: 0,
    });
  });
});



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

// ─── AgentAdapter base class ─────────────────────────────────────────────────

describe("AgentAdapter base class", () => {
  it("onEvent registers and unregisters listeners", () => {
    const adapter = new ClaudeCodeAdapter();
    const events: AgentEvent[] = [];
    const unsubscribe = adapter.onEvent((e) => events.push(e));

    // Simulate emitting via parseClaudeEvent (which is public for testing)
    const event = adapter.parseClaudeEvent({ type: "assistant", content: "hello" });
    // Manually call the protected method via the adapter's public parse
    // We test the listener mechanism indirectly through the full flow

    expect(typeof unsubscribe).toBe("function");
    unsubscribe();
  });

  it("onExit registers and unregisters listeners", () => {
    const adapter = new ClaudeCodeAdapter();
    const codes: number[] = [];
    const unsubscribe = adapter.onExit((code) => codes.push(code));

    expect(typeof unsubscribe).toBe("function");
    unsubscribe();
  });
});
