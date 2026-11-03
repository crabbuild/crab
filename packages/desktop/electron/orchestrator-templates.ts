/**
 * Workspace Templates for the Crab Desktop Orchestrator.
 *
 * Manages template CRUD operations, loading from multiple sources
 * (built-in, repo-level, user-level), validation, and resolution
 * (merging template defaults with user overrides).
 */

import * as fs from "node:fs";
import * as path from "node:path";
import * as os from "node:os";

// ─── Types ───────────────────────────────────────────────────────────────────

export interface WorkspaceTemplate {
  name: string;
  description: string;
  icon?: string;
  agentType: "claude-code" | "codex" | "custom";
  model?: string;
  planMode: boolean;
  fastMode?: boolean;
  contextFiles?: string[];
  preferences?: string;
  pipeline?: string;
  autoFix?: boolean;
  budgetLimit?: number;
  baseBranch?: string;
  source: "builtin" | "repo" | "user";
  skills?: string[];
}

// ─── Built-in Defaults ───────────────────────────────────────────────────────

const BUILTIN_TEMPLATES: WorkspaceTemplate[] = [
  {
    name: "Bug Fix",
    description: "Quick fix with auto-testing",
    icon: "🐛",
    agentType: "claude-code",
    planMode: false,
    fastMode: true,
    autoFix: true,
    source: "builtin",
  },
  {
    name: "Feature",
    description: "Plan-first feature implementation",
    icon: "✨",
    agentType: "claude-code",
    planMode: true,
    contextFiles: ["docs/architecture.md"],
    source: "builtin",
  },
  {
    name: "Refactor",
    description: "Restructure existing code",
    icon: "♻️",
    agentType: "claude-code",
    planMode: true,
    source: "builtin",
  },
];

// ─── Validation ──────────────────────────────────────────────────────────────

const VALID_AGENT_TYPES = new Set(["claude-code", "codex", "custom"]);

/**
 * Validates that a template object has all required fields and correct types.
 * Returns null if valid, or an error message describing the problem.
 */
function validateTemplate(raw: unknown): string | null {
  if (typeof raw !== "object" || raw === null) {
    return "template must be an object";
  }

  const obj = raw as Record<string, unknown>;

  if (typeof obj.name !== "string" || obj.name.trim() === "") {
    return "template.name must be a non-empty string";
  }

  if (!VALID_AGENT_TYPES.has(obj.agentType as string)) {
    return `template.agentType must be one of: ${[...VALID_AGENT_TYPES].join(", ")}`;
  }

  if (typeof obj.planMode !== "boolean") {
    return "template.planMode must be a boolean";
  }

  return null;
}

// ─── TemplateManager ─────────────────────────────────────────────────────────

const USER_TEMPLATES_DIR = path.join(
  os.homedir(),
  ".crab",
  "desktop",
  "templates",
);

export class TemplateManager {
  private templates = new Map<string, WorkspaceTemplate>();

  /**
   * Loads templates from all sources: built-in defaults, repo-level
   * (`crab-orchestrator.json`), and user-level (`~/.crab/desktop/templates/`).
   *
   * Precedence on name conflicts: repo > user > builtin.
   */
  loadTemplates(repoPath: string): void {
    this.templates.clear();

    // 1. Load built-in templates (lowest precedence).
    for (const t of BUILTIN_TEMPLATES) {
      this.templates.set(t.name, t);
    }

    // 2. Load user-level templates (overrides built-in).
    const userTemplates = this.loadUserTemplates();
    for (const t of userTemplates) {
      this.templates.set(t.name, t);
    }

    // 3. Load repo-level templates (highest precedence).
    const repoTemplates = this.loadRepoTemplates(repoPath);
    for (const t of repoTemplates) {
      this.templates.set(t.name, t);
    }
  }

  /** Returns all loaded templates. */
  listTemplates(): WorkspaceTemplate[] {
    return Array.from(this.templates.values());
  }

  /** Returns a specific template by name, or undefined if not found. */
  getTemplate(name: string): WorkspaceTemplate | undefined {
    return this.templates.get(name);
  }

  /**
   * Saves a template to user-level storage (`~/.crab/desktop/templates/`).
   * The template's source is set to "user" regardless of input.
   */
  saveTemplate(template: WorkspaceTemplate): void {
    const toSave: WorkspaceTemplate = { ...template, source: "user" };

    const error = validateTemplate(toSave);
    if (error) {
      throw new Error(`Invalid template: ${error}`);
    }

    try {
      fs.mkdirSync(USER_TEMPLATES_DIR, { recursive: true });
    } catch {
      // Directory may already exist.
    }

    const filename = this.templateFilename(toSave.name);
    const filePath = path.join(USER_TEMPLATES_DIR, filename);

    // Write without the `source` field — it's inferred on load.
    const { source: _, ...data } = toSave;
    fs.writeFileSync(filePath, JSON.stringify(data, null, 2), "utf-8");

    // Update in-memory map.
    this.templates.set(toSave.name, toSave);
  }

  /**
   * Deletes a template from user-level storage. Only user-level templates
   * can be deleted. Returns true if deleted, false if not found or not user-level.
   */
  deleteTemplate(name: string): boolean {
    const existing = this.templates.get(name);
    if (!existing || existing.source !== "user") {
      return false;
    }

    const filename = this.templateFilename(name);
    const filePath = path.join(USER_TEMPLATES_DIR, filename);

    try {
      if (fs.existsSync(filePath)) {
        fs.unlinkSync(filePath);
      }
    } catch {
      // Best-effort deletion.
    }

    this.templates.delete(name);
    return true;
  }

  /**
   * Resolves a template by merging its defaults with user overrides.
   * Overrides take precedence for any field that is explicitly provided.
   */
  resolveTemplate(
    templateName: string,
    overrides: Partial<WorkspaceTemplate>,
  ): WorkspaceTemplate | undefined {
    const base = this.templates.get(templateName);
    if (!base) return undefined;

    const resolved: WorkspaceTemplate = { ...base };

    for (const [key, value] of Object.entries(overrides)) {
      if (value !== undefined) {
        (resolved as unknown as Record<string, unknown>)[key] = value;
      }
    }

    return resolved;
  }

  // ─── Private Helpers ─────────────────────────────────────────────────────

  /**
   * Loads templates from `crab-orchestrator.json` at the repo root.
   * Invalid templates are skipped with a console warning.
   */
  private loadRepoTemplates(repoPath: string): WorkspaceTemplate[] {
    const configPath = path.join(repoPath, "crab-orchestrator.json");
    const results: WorkspaceTemplate[] = [];

    try {
      if (!fs.existsSync(configPath)) return results;

      const raw = fs.readFileSync(configPath, "utf-8");
      const config = JSON.parse(raw);

      if (!config.templates || !Array.isArray(config.templates)) {
        return results;
      }

      for (const entry of config.templates) {
        const error = validateTemplate(entry);
        if (error) {
          console.warn(
            `[orchestrator-templates] Skipping invalid repo template: ${error}`,
          );
          continue;
        }

        results.push({
          ...entry,
          description: entry.description ?? "",
          source: "repo",
        });
      }
    } catch {
      // If the file is malformed JSON, skip repo templates entirely.
    }

    return results;
  }

  /**
   * Loads templates from `~/.crab/desktop/templates/*.json`.
   * Each JSON file represents one template.
   */
  private loadUserTemplates(): WorkspaceTemplate[] {
    const results: WorkspaceTemplate[] = [];

    try {
      if (!fs.existsSync(USER_TEMPLATES_DIR)) return results;

      const files = fs.readdirSync(USER_TEMPLATES_DIR).filter((f) =>
        f.endsWith(".json"),
      );

      for (const file of files) {
        try {
          const filePath = path.join(USER_TEMPLATES_DIR, file);
          const raw = fs.readFileSync(filePath, "utf-8");
          const data = JSON.parse(raw);

          const error = validateTemplate(data);
          if (error) {
            console.warn(
              `[orchestrator-templates] Skipping invalid user template "${file}": ${error}`,
            );
            continue;
          }

          results.push({
            ...data,
            description: data.description ?? "",
            source: "user",
          });
        } catch {
          // Skip individual malformed files.
        }
      }
    } catch {
      // If the directory can't be read, return empty.
    }

    return results;
  }

  /**
   * Converts a template name to a safe filename.
   * e.g., "Bug Fix" → "bug-fix.json"
   */
  private templateFilename(name: string): string {
    return (
      name
        .toLowerCase()
        .replace(/[^a-z0-9]+/g, "-")
        .replace(/^-|-$/g, "") + ".json"
    );
  }
}
