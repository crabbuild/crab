/**
 * Tests for `orchestrator-settings.ts`: per-agent config round-trip
 * persistence and the legacy-settings migration.
 *
 * The module honours the `CRAB_DESKTOP_SETTINGS_PATH` env var so tests
 * can redirect reads/writes to an isolated temp directory.
 */

import { describe, it, expect, beforeEach, afterEach } from "vitest";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import fc from "fast-check";

import {
  loadAgentConfig,
  migrateSettings,
  readSettings,
  saveAgentConfig,
  writeSettings,
} from "../electron/orchestrator-settings";

// ─── Sandbox setup ───────────────────────────────────────────────────────────

let sandboxDir: string;
let sandboxPath: string;
let prevEnv: string | undefined;

beforeEach(() => {
  sandboxDir = fs.mkdtempSync(path.join(os.tmpdir(), "crab-settings-test-"));
  sandboxPath = path.join(sandboxDir, "settings.json");
  prevEnv = process.env.CRAB_DESKTOP_SETTINGS_PATH;
  process.env.CRAB_DESKTOP_SETTINGS_PATH = sandboxPath;
});

afterEach(() => {
  if (prevEnv === undefined) delete process.env.CRAB_DESKTOP_SETTINGS_PATH;
  else process.env.CRAB_DESKTOP_SETTINGS_PATH = prevEnv;
  try {
    fs.rmSync(sandboxDir, { recursive: true, force: true });
  } catch { /* ignore */ }
});

// ─── Settings round-trip ─────────────────────────────────────────────────────

describe("orchestrator-settings", () => {
  it("loadAgentConfig returns empty object when no settings file exists", () => {
    expect(loadAgentConfig("claude-code")).toEqual({});
  });

  it.each(["__proto__", "constructor", "toString"])("loads no inherited config for %s", (agentType) => {
    expect(loadAgentConfig(agentType)).toEqual({});
  });

  it.each(["__proto__", "constructor", "toString"])("persists the literal agent identifier %s", (agentType) => {
    const config = { binaryPath: "/custom/agent", enabled: false };
    saveAgentConfig(agentType, config);

    expect(loadAgentConfig(agentType)).toEqual(config);
  });

  it("saving agent configuration preserves preferences and other agents", () => {
    writeSettings({ theme: "dark", orchestratorAgentConfigs: { codex: { enabled: true } } });
    saveAgentConfig("custom", { enabled: false });

    expect(readSettings()).toEqual({
      theme: "dark", orchestratorAgentConfigs: { codex: { enabled: true }, custom: { enabled: false } },
    });
  });

  it("Feature: multi-agent-support, Property 10: agent settings round-trip persistence", () => {
    fc.assert(
      fc.property(
        fc.record({
          binaryPath: fc.option(fc.string({ minLength: 1, maxLength: 100 }), { nil: undefined }),
          extraArgs: fc.option(fc.array(fc.string(), { maxLength: 5 }), { nil: undefined }),
          env: fc.option(
            fc.dictionary(
              fc.string({ minLength: 1, maxLength: 20, unit: "grapheme-ascii" }).filter((s) => /^[A-Z_]+$/.test(s)),
              fc.string({ maxLength: 50 }),
            ),
            { nil: undefined },
          ),
          enabled: fc.option(fc.boolean(), { nil: undefined }),
        }),
        fc.string({ minLength: 1, maxLength: 20 }).filter((s) => s.length > 0),
        (config, agentType) => {
          const settings = readSettings();
          const configs = (settings.orchestratorAgentConfigs ?? {}) as Record<string, unknown>;
          configs[agentType] = config;
          settings.orchestratorAgentConfigs = configs;
          writeSettings(settings);

          const loaded = loadAgentConfig(agentType);
          expect(loaded).toEqual(config);
        },
      ),
      { numRuns: 50 },
    );
  });

  it("migrateSettings migrates legacy orchestratorClaudeCodePath", () => {
    writeSettings({
      orchestratorClaudeCodePath: "/custom/path/claude",
      orchestratorCodexPath: "/custom/path/codex",
      orchestratorCustomBinaryPath: "/custom/path/my-agent",
    });

    migrateSettings();

    expect(loadAgentConfig("claude-code").binaryPath).toBe("/custom/path/claude");
    expect(loadAgentConfig("codex").binaryPath).toBe("/custom/path/codex");
    expect(loadAgentConfig("custom").binaryPath).toBe("/custom/path/my-agent");

    // Legacy keys preserved (Requirement 9.2 — downgrade safety).
    const settings = readSettings();
    expect(settings.orchestratorClaudeCodePath).toBe("/custom/path/claude");
    expect(settings.orchestratorAgentConfigsMigrationV1).toBe(true);
  });

  it("migrateSettings is idempotent — running twice produces the same result", () => {
    writeSettings({ orchestratorClaudeCodePath: "/path/claude" });
    migrateSettings();
    const after1 = readSettings();
    migrateSettings();
    const after2 = readSettings();
    expect(after2).toEqual(after1);
  });

  it("migrateSettings does not overwrite an existing config", () => {
    writeSettings({
      orchestratorClaudeCodePath: "/legacy/claude",
      orchestratorAgentConfigs: {
        "claude-code": { binaryPath: "/already/set", extraArgs: ["--foo"] },
      },
    });
    migrateSettings();
    const loaded = loadAgentConfig("claude-code");
    expect(loaded.binaryPath).toBe("/already/set");
    expect(loaded.extraArgs).toEqual(["--foo"]);
  });

  it("migrateSettings handles missing legacy keys gracefully", () => {
    migrateSettings();
    const settings = readSettings();
    expect(settings.orchestratorAgentConfigsMigrationV1).toBe(true);
    expect(settings.orchestratorAgentConfigs).toEqual({});
  });
});
