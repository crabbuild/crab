/**
 * Unit tests for the agent context builder module.
 *
 * Tests extractFilePaths regex matching and buildAgentContext prompt
 * construction including truncation behavior and empty-description handling.
 */

import { describe, it, expect } from "vitest";

import {
  extractFilePaths,
  buildAgentContext,
  type AgentContextInput,
} from "../electron/kanban/agent-context";

describe("extractFilePaths", () => {
  it("extracts simple relative paths with extensions", () => {
    const text = "Look at src/foo/bar.ts and lib/utils.py for details";
    const paths = extractFilePaths(text);
    expect(paths).toContain("src/foo/bar.ts");
    expect(paths).toContain("lib/utils.py");
  });

  it("extracts paths with ./ prefix", () => {
    const text = "Check ./src/components/Card.tsx";
    const paths = extractFilePaths(text);
    expect(paths).toContain("./src/components/Card.tsx");
  });

  it("extracts paths with ../ prefix", () => {
    const text = "See ../electron/kanban/file.ts";
    const paths = extractFilePaths(text);
    expect(paths).toContain("../electron/kanban/file.ts");
  });

  it("extracts deeply nested paths", () => {
    const text = "electron/kanban/tracker-adapters/github.ts is relevant";
    const paths = extractFilePaths(text);
    expect(paths).toContain("electron/kanban/tracker-adapters/github.ts");
  });

  it("deduplicates repeated paths", () => {
    const text = "src/foo/bar.ts and again src/foo/bar.ts";
    const paths = extractFilePaths(text);
    expect(paths).toEqual(["src/foo/bar.ts"]);
  });

  it("returns empty array for text without file paths", () => {
    const text = "No file paths here, just plain text.";
    const paths = extractFilePaths(text);
    expect(paths).toEqual([]);
  });

  it("returns empty array for empty string", () => {
    expect(extractFilePaths("")).toEqual([]);
  });

  it("handles paths with dots in directory names", () => {
    const text = "Check node_modules/@dnd-kit/core/dist/index.js";
    const paths = extractFilePaths(text);
    expect(paths).toContain("node_modules/@dnd-kit/core/dist/index.js");
  });

  it("handles paths with hyphens and underscores", () => {
    const text = "See src/my-component/sub_module/file.tsx";
    const paths = extractFilePaths(text);
    expect(paths).toContain("src/my-component/sub_module/file.tsx");
  });
});

describe("buildAgentContext", () => {
  const baseInput: AgentContextInput = {
    cardTitle: "Fix login bug",
    cardDescription: "The login form in src/auth/login.ts fails when email contains a +",
    repoPath: "/home/user/project",
    labels: ["bug", "auth"],
  };

  it("constructs prompt with all sections", () => {
    const result = buildAgentContext(baseInput);

    expect(result.prompt).toContain("## Repository\n/home/user/project");
    expect(result.prompt).toContain("## Labels\n- bug\n- auth");
    expect(result.prompt).toContain("## Related Files\n- src/auth/login.ts");
    expect(result.prompt).toContain("## Task\nFix login bug");
    expect(result.prompt).toContain(baseInput.cardDescription);
  });

  it("returns correct repoPath", () => {
    const result = buildAgentContext(baseInput);
    expect(result.repoPath).toBe("/home/user/project");
  });

  it("returns extracted file paths", () => {
    const result = buildAgentContext(baseInput);
    expect(result.relatedFiles).toContain("src/auth/login.ts");
  });

  it("omits Related Files section when description is empty", () => {
    const input: AgentContextInput = {
      ...baseInput,
      cardDescription: "",
    };
    const result = buildAgentContext(input);

    expect(result.prompt).not.toContain("## Related Files");
    expect(result.prompt).toContain("## Repository");
    expect(result.prompt).toContain("## Labels");
    expect(result.prompt).toContain("## Task");
  });

  it("omits Related Files section when description has no file paths", () => {
    const input: AgentContextInput = {
      ...baseInput,
      cardDescription: "Just a plain description with no paths",
    };
    const result = buildAgentContext(input);

    expect(result.prompt).not.toContain("## Related Files");
    expect(result.prompt).toContain("Just a plain description with no paths");
  });

  it("enforces 8000-character limit by truncating description", () => {
    const longDescription = "x".repeat(10000) + " src/foo/bar.ts";
    const input: AgentContextInput = {
      ...baseInput,
      cardDescription: longDescription,
    };
    const result = buildAgentContext(input);

    expect(result.prompt.length).toBeLessThanOrEqual(8000);
    // Metadata sections are preserved
    expect(result.prompt).toContain("## Repository\n/home/user/project");
    expect(result.prompt).toContain("## Labels\n- bug\n- auth");
    expect(result.prompt).toContain("## Task\nFix login bug");
  });

  it("preserves Related Files section even when description is truncated", () => {
    const longDescription = "Check src/auth/login.ts for the issue. " + "x".repeat(10000);
    const input: AgentContextInput = {
      ...baseInput,
      cardDescription: longDescription,
    };
    const result = buildAgentContext(input);

    expect(result.prompt.length).toBeLessThanOrEqual(8000);
    expect(result.prompt).toContain("## Related Files\n- src/auth/login.ts");
    expect(result.relatedFiles).toContain("src/auth/login.ts");
  });

  it("handles empty labels array", () => {
    const input: AgentContextInput = {
      ...baseInput,
      labels: [],
    };
    const result = buildAgentContext(input);

    expect(result.prompt).toContain("## Labels\n(none)");
  });

  it("prompt sections appear in correct order", () => {
    const result = buildAgentContext(baseInput);

    const repoIdx = result.prompt.indexOf("## Repository");
    const labelsIdx = result.prompt.indexOf("## Labels");
    const filesIdx = result.prompt.indexOf("## Related Files");
    const taskIdx = result.prompt.indexOf("## Task");

    expect(repoIdx).toBeLessThan(labelsIdx);
    expect(labelsIdx).toBeLessThan(filesIdx);
    expect(filesIdx).toBeLessThan(taskIdx);
  });
});
