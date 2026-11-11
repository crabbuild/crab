import { describe, expect, it } from "vitest";
import { buildAgentModelFlags } from "../electron/orchestrator-steering";

describe("official agent model controls", () => {
  it("uses app-server configuration overrides for Codex", () => {
    expect(buildAgentModelFlags("codex", { model: "selected-model", reasoningLevel: "xhigh" })).toEqual([
      "-c", 'model="selected-model"', "-c", 'model_reasoning_effort="xhigh"',
    ]);
  });

  it("uses Claude effort without accidentally continuing another session", () => {
    expect(buildAgentModelFlags("claude-code", { model: "sonnet", reasoningLevel: "max" })).toEqual([
      "--model", "sonnet", "--effort", "max",
    ]);
  });
});
