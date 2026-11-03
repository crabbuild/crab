/**
 * Repository Preferences for the Crab Desktop Orchestrator.
 *
 * Loads preferences from `.crab/orchestrator/preferences.md` and
 * detects existing instruction files (AGENTS.md, CLAUDE.md, etc.)
 * for injection into agent prompts.
 */

import * as fs from "node:fs";
import * as path from "node:path";

// ─── Types ───────────────────────────────────────────────────────────────────

export interface OrchestratorPreferences {
  general: string;
  codeReview: string;
  pr: string;
  conflictResolution: string;
}

export interface InstructionFile {
  name: string;
  path: string;
  content: string;
}

export interface MCPServerConfig {
  name: string;
  command: string;
  args?: string[];
  env?: Record<string, string>;
}

// ─── Preferences Loading ─────────────────────────────────────────────────────

/**
 * Loads orchestrator preferences from `.crab/orchestrator/preferences.md`.
 * Returns null if the file doesn't exist.
 */
export function loadPreferencesFromFile(repoPath: string): OrchestratorPreferences | null {
  const prefsPath = path.join(repoPath, ".crab", "orchestrator", "preferences.md");

  try {
    if (!fs.existsSync(prefsPath)) return null;
    const content = fs.readFileSync(prefsPath, "utf-8");
    return parsePreferencesMarkdown(content);
  } catch {
    return null;
  }
}

/**
 * Parses a preferences markdown file into structured sections.
 * Expected format:
 * ```
 * ## General
 * ...content...
 *
 * ## Code Review
 * ...content...
 *
 * ## PR
 * ...content...
 *
 * ## Conflict Resolution
 * ...content...
 * ```
 */
function parsePreferencesMarkdown(content: string): OrchestratorPreferences {
  const sections: Record<string, string> = {};
  let currentSection = "general";
  const lines = content.split("\n");

  for (const line of lines) {
    const headerMatch = line.match(/^##\s+(.+)/);
    if (headerMatch) {
      const header = headerMatch[1].trim().toLowerCase();
      if (header.includes("general")) currentSection = "general";
      else if (header.includes("code review") || header.includes("review")) currentSection = "codeReview";
      else if (header.includes("pr") || header.includes("pull request")) currentSection = "pr";
      else if (header.includes("conflict")) currentSection = "conflictResolution";
      else currentSection = header;
    } else {
      sections[currentSection] = (sections[currentSection] ?? "") + line + "\n";
    }
  }

  return {
    general: (sections["general"] ?? "").trim(),
    codeReview: (sections["codeReview"] ?? "").trim(),
    pr: (sections["pr"] ?? "").trim(),
    conflictResolution: (sections["conflictResolution"] ?? "").trim(),
  };
}

// ─── Instruction File Detection ──────────────────────────────────────────────

/**
 * Detects and loads existing instruction files from the repository.
 * Looks for: AGENTS.md, CLAUDE.md, .claude/commands, .cursorrules, etc.
 */
export function detectInstructionFiles(repoPath: string): InstructionFile[] {
  const files: InstructionFile[] = [];

  const candidates = [
    { name: "AGENTS.md", relativePath: "AGENTS.md" },
    { name: "CLAUDE.md", relativePath: "CLAUDE.md" },
    { name: ".claude/commands", relativePath: ".claude/commands" },
    { name: ".cursorrules", relativePath: ".cursorrules" },
    { name: ".github/copilot-instructions.md", relativePath: ".github/copilot-instructions.md" },
    { name: "CONTRIBUTING.md", relativePath: "CONTRIBUTING.md" },
  ];

  for (const candidate of candidates) {
    const fullPath = path.join(repoPath, candidate.relativePath);
    try {
      if (fs.existsSync(fullPath)) {
        const stat = fs.statSync(fullPath);
        if (stat.isFile()) {
          const content = fs.readFileSync(fullPath, "utf-8");
          // Limit content to 10KB to avoid bloating prompts
          files.push({
            name: candidate.name,
            path: candidate.relativePath,
            content: content.slice(0, 10_000),
          });
        } else if (stat.isDirectory()) {
          // For directories like .claude/commands, read all files inside
          const entries = fs.readdirSync(fullPath);
          for (const entry of entries.slice(0, 10)) {
            const entryPath = path.join(fullPath, entry);
            const entryStat = fs.statSync(entryPath);
            if (entryStat.isFile()) {
              const content = fs.readFileSync(entryPath, "utf-8");
              files.push({
                name: `${candidate.name}/${entry}`,
                path: `${candidate.relativePath}/${entry}`,
                content: content.slice(0, 5_000),
              });
            }
          }
        }
      }
    } catch {
      // Skip files that can't be read
    }
  }

  return files;
}

// ─── Preference Injection ────────────────────────────────────────────────────

/**
 * Builds a preference context string to append to agent prompts.
 * Selects relevant preferences based on the context (e.g., review vs PR creation).
 */
export function buildPreferenceContext(
  preferences: OrchestratorPreferences | null,
  instructionFiles: InstructionFile[],
  context: "general" | "review" | "pr" | "conflict",
): string {
  const parts: string[] = [];

  // Add instruction files
  if (instructionFiles.length > 0) {
    parts.push("## Repository Instructions\n");
    for (const file of instructionFiles) {
      parts.push(`### ${file.name}\n${file.content}\n`);
    }
  }

  // Add relevant preferences
  if (preferences) {
    if (preferences.general) {
      parts.push(`## General Preferences\n${preferences.general}\n`);
    }

    switch (context) {
      case "review":
        if (preferences.codeReview) {
          parts.push(`## Code Review Preferences\n${preferences.codeReview}\n`);
        }
        break;
      case "pr":
        if (preferences.pr) {
          parts.push(`## PR Preferences\n${preferences.pr}\n`);
        }
        break;
      case "conflict":
        if (preferences.conflictResolution) {
          parts.push(`## Conflict Resolution Preferences\n${preferences.conflictResolution}\n`);
        }
        break;
    }
  }

  return parts.join("\n");
}

// ─── MCP Configuration ───────────────────────────────────────────────────────

/**
 * Loads MCP server configuration from `.mcp.json` in the repository.
 */
export function loadMCPConfig(repoPath: string): MCPServerConfig[] {
  const mcpPath = path.join(repoPath, ".mcp.json");

  try {
    if (!fs.existsSync(mcpPath)) return [];
    const content = fs.readFileSync(mcpPath, "utf-8");
    const config = JSON.parse(content);

    if (config.mcpServers && typeof config.mcpServers === "object") {
      return Object.entries(config.mcpServers).map(([name, serverConfig]: [string, any]) => ({
        name,
        command: serverConfig.command ?? "",
        args: serverConfig.args,
        env: serverConfig.env,
      }));
    }

    if (Array.isArray(config.servers)) {
      return config.servers.map((s: any) => ({
        name: s.name ?? "unknown",
        command: s.command ?? "",
        args: s.args,
        env: s.env,
      }));
    }

    return [];
  } catch {
    return [];
  }
}

// ─── Workspace Context ───────────────────────────────────────────────────────

/**
 * Creates the `.context/` folder in a workspace directory.
 */
export function createContextFolder(worktreePath: string): void {
  const contextPath = path.join(worktreePath, ".context");
  try {
    if (!fs.existsSync(contextPath)) {
      fs.mkdirSync(contextPath, { recursive: true });
    }
    // Add .gitignore to keep it out of version control
    const gitignorePath = path.join(contextPath, ".gitignore");
    if (!fs.existsSync(gitignorePath)) {
      fs.writeFileSync(gitignorePath, "*\n", "utf-8");
    }
  } catch {
    // Best-effort
  }
}

/**
 * Adds a file to the workspace `.context/` folder.
 */
export function addContextFile(worktreePath: string, fileName: string, content: string | Buffer): void {
  const contextPath = path.join(worktreePath, ".context");
  createContextFolder(worktreePath);
  const filePath = path.join(contextPath, fileName);
  fs.writeFileSync(filePath, content);
}

/**
 * Removes a file from the workspace `.context/` folder. Idempotent —
 * missing files are treated as success so the UI can call this blindly
 * without racing against concurrent edits.
 */
export function removeContextFile(worktreePath: string, fileName: string): void {
  // Resolve inside .context/ and reject anything that escapes (path traversal guard).
  const contextPath = path.resolve(path.join(worktreePath, ".context"));
  const filePath = path.resolve(path.join(contextPath, fileName));
  if (!filePath.startsWith(contextPath + path.sep)) return;
  try {
    fs.unlinkSync(filePath);
  } catch (err: any) {
    if (err?.code !== "ENOENT") throw err;
  }
}

/**
 * Lists files in the workspace `.context/` folder with their content
 * previews. Returns tuples the UI can render directly without a second
 * round-trip per file.
 */
export function listContextFiles(
  worktreePath: string,
): Array<{ name: string; content: string; size: number }> {
  const contextPath = path.join(worktreePath, ".context");
  try {
    if (!fs.existsSync(contextPath)) return [];
    const entries = fs.readdirSync(contextPath).filter((f) => f !== ".gitignore");
    return entries.map((name) => {
      const filePath = path.join(contextPath, name);
      try {
        const stat = fs.statSync(filePath);
        if (!stat.isFile()) return { name, content: "", size: 0 };
        // Cap preview to 8 KiB — context files can be large and we only
        // render the first ~120 chars anyway.
        const fd = fs.openSync(filePath, "r");
        const buf = Buffer.alloc(Math.min(8192, stat.size));
        fs.readSync(fd, buf, 0, buf.length, 0);
        fs.closeSync(fd);
        return { name, content: buf.toString("utf-8"), size: stat.size };
      } catch {
        return { name, content: "", size: 0 };
      }
    });
  } catch {
    return [];
  }
}
