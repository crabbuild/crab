/**
 * Skill Manager — canonical skills directory CRUD + agent projection.
 *
 * Skills live in ~/.crab/desktop/skills/<id>/ (user scope) or
 * <repo>/.crab/skills/<id>/ (repo scope). The SkillManager
 * transparently syncs symlinks into each agent's expected directory
 * so skills "just show up" in every installed agent.
 */

import * as fs from "node:fs";
import * as path from "node:path";
import * as os from "node:os";
import { createHash } from "node:crypto";
import { SkillDatabase } from "./skill-manager-db";
import { agentTargets } from "./skill-agent-targets";
import {
  parseFrontmatter,
  generateSkillMd,
  isValidSkillName,
} from "./skill-validation";
import type {
  SkillRecord,
  SkillListItem,
  SkillDetail,
  SkillScope,
  DetectedAgent,
  SkillOpResult,
  SkillActivationPrefs,
} from "./skill-manager-types";

const USER_SKILLS_DIR = path.join(os.homedir(), ".crab", "desktop", "skills");

function ok<T>(data: T): SkillOpResult<T> {
  return { success: true, data };
}

function err<T>(error: string): SkillOpResult<T> {
  return { success: false, error };
}

export class SkillManager {
  private db: SkillDatabase;
  private userSkillsDir: string;

  constructor() {
    this.userSkillsDir = USER_SKILLS_DIR;
    this.db = new SkillDatabase();
  }

  async initialize(): Promise<void> {
    await fs.promises.mkdir(this.userSkillsDir, { recursive: true });
    await this.reconcile();
  }

  // ── CRUD ──────────────────────────────────────────────────────────

  async createSkill(
    name: string,
    description: string,
    body?: string,
    scope: SkillScope = "user",
    repoPath?: string,
  ): Promise<SkillOpResult<SkillDetail>> {
    if (!isValidSkillName(name)) {
      return err(
        "Invalid skill name. Use lowercase letters, numbers, and hyphens (1-64 chars).",
      );
    }

    const canonicalDir = this.getSkillCanonicalPath(name, scope, repoPath);

    try {
      await fs.promises.access(canonicalDir);
      return err(`Skill "${name}" already exists`);
    } catch (e) {
      if ((e as NodeJS.ErrnoException).code !== "ENOENT") throw e;
    }

    const existing = this.db.getSkill(name);
    if (existing) {
      return err(`Skill "${name}" already exists`);
    }

    const content = generateSkillMd(name, description, body);
    const { frontmatter, body: parsedBody } = parseFrontmatter(content);
    const contentHash = this.hashContent(parsedBody);
    const now = Date.now();

    try {
      await fs.promises.mkdir(canonicalDir, { recursive: true });
      await fs.promises.writeFile(
        path.join(canonicalDir, "SKILL.md"),
        content,
      );

      const record: SkillRecord = {
        id: name,
        name: frontmatter.name || name,
        description: frontmatter.description || description,
        scope,
        repoPath: repoPath ?? null,
        sourceUrl: null,
        installedAt: now,
        updatedAt: now,
        contentHash,
        enabled: true,
        activationPrefs: {
          autoActivate: false,
          restrictToAgents: [],
        },
      };

      this.db.upsertSkill(record);
      await this.syncToAgents(name, scope, repoPath);

      const agentStatus = await this.computeAgentStatus(name, scope, repoPath);
      return ok({
        record,
        frontmatter,
        body: parsedBody,
        agentStatus,
      });
    } catch (e) {
      // Cleanup on failure
      await fs.promises.rm(canonicalDir, { recursive: true, force: true }).catch(() => {});
      return err(`Failed to create skill: ${(e as Error).message}`);
    }
  }

  async updateSkill(
    id: string,
    updates: { name?: string; description?: string; body?: string },
  ): Promise<SkillOpResult<SkillDetail>> {
    const existing = this.db.getSkill(id);
    if (!existing) {
      return err(`Skill "${id}" not found`);
    }

    const canonicalDir = this.getSkillCanonicalPath(
      id,
      existing.scope,
      existing.repoPath ?? undefined,
    );

    try {
      // Read current SKILL.md to presreve body if not updating
      let currentContent = "";
      try {
        currentContent = await fs.promises.readFile(
          path.join(canonicalDir, "SKILL.md"),
          "utf-8",
        );
      } catch {
        // File missing — regenerate from DB data
      }

      const { body: currentBody } = parseFrontmatter(currentContent);
      const newName = updates.name ?? existing.name;
      const newDesc = updates.description ?? existing.description;
      const newBody = updates.body ?? currentBody;

      const content = generateSkillMd(newName, newDesc, newBody);
      const { frontmatter, body: parsedBody } = parseFrontmatter(content);
      const newHash = this.hashContent(parsedBody);
      const hashChanged = newHash !== existing.contentHash;

      await fs.promises.writeFile(
        path.join(canonicalDir, "SKILL.md"),
        content,
      );

      const record: SkillRecord = {
        ...existing,
        name: frontmatter.name || newName,
        description: frontmatter.description || newDesc,
        updatedAt: Date.now(),
        contentHash: newHash,
      };

      this.db.upsertSkill(record);

      if (hashChanged) {
        await this.syncToAgents(id, record.scope, record.repoPath ?? undefined);
      }

      const agentStatus = await this.computeAgentStatus(
        id,
        record.scope,
        record.repoPath ?? undefined,
      );
      return ok({
        record,
        frontmatter,
        body: parsedBody,
        agentStatus,
      });
    } catch (e) {
      return err(`Failed to update skill: ${(e as Error).message}`);
    }
  }

  async deleteSkill(
    id: string,
  ): Promise<SkillOpResult<void>> {
    const existing = this.db.getSkill(id);
    if (!existing) {
      return err(`Skill "${id}" not found`);
    }

    const canonicalDir = this.getSkillCanonicalPath(
      id,
      existing.scope,
      existing.repoPath ?? undefined,
    );

    try {
      // Remove agent symlinks first
      await this.unsyncFromAgents(id);

      // Remove canonical directory
      await fs.promises.rm(canonicalDir, { recursive: true, force: true });

      // Remove from DB
      this.db.deleteSkill(id);

      return ok(undefined);
    } catch (e) {
      return err(`Failed to delete skill: ${(e as Error).message}`);
    }
  }

  async getSkill(
    id: string,
  ): Promise<SkillOpResult<SkillDetail>> {
    const record = this.db.getSkill(id);
    if (!record) {
      return err(`Skill "${id}" not found`);
    }

    const canonicalDir = this.getSkillCanonicalPath(
      id,
      record.scope,
      record.repoPath ?? undefined,
    );

    try {
      const content = await fs.promises.readFile(
        path.join(canonicalDir, "SKILL.md"),
        "utf-8",
      );
      const { frontmatter, body } = parseFrontmatter(content);
      const agentStatus = await this.computeAgentStatus(
        id,
        record.scope,
        record.repoPath ?? undefined,
      );

      return ok({
        record,
        frontmatter,
        body,
        agentStatus,
      });
    } catch (e) {
      return err(
        `Failed to read skill "${id}": ${(e as Error).message}`,
      );
    }
  }

  async listSkills(opts?: {
    scope?: SkillScope;
    repoPath?: string;
  }): Promise<SkillOpResult<SkillListItem[]>> {
    try {
      const records = this.db.listSkills(opts);
      const items: SkillListItem[] = [];

      for (const record of records) {
        const canonicalDir = this.getSkillCanonicalPath(
          record.id,
          record.scope,
          record.repoPath ?? undefined,
        );

        let frontmatter = { name: record.name, description: record.description };
        let bodyPreview = "";

        try {
          const content = await fs.promises.readFile(
            path.join(canonicalDir, "SKILL.md"),
            "utf-8",
          );
          const parsed = parseFrontmatter(content);
          frontmatter = parsed.frontmatter;
          bodyPreview = parsed.body.slice(0, 200);
        } catch {
          // Use DB values as fallback
        }

        const agentStatus = await this.computeAgentStatus(
          record.id,
          record.scope,
          record.repoPath ?? undefined,
        );

        items.push({
          record,
          frontmatter,
          bodyPreview,
          agentStatus,
        });
      }

      return ok(items);
    } catch (e) {
      return err(`Failed to list skills: ${(e as Error).message}`);
    }
  }

  async installSkillFromUrl(
    url: string,
    scope: SkillScope = "user",
    repoPath?: string,
  ): Promise<SkillOpResult<SkillDetail>> {
    const raw = this.githubUrlToRaw(url);
    if (!raw) {
      return err(
        "Could not parse the URL. Use a GitHub blob/tree URL, raw.githubusercontent URL, or `owner/repo/path/to/SKILL.md`.",
      );
    }

    let response: Response;
    try {
      response = await fetch(raw);
    } catch (e) {
      return err(`Network error: ${(e as Error).message}`);
    }
    if (!response.ok) {
      return err(
        `Fetch failed with HTTP ${response.status} ${response.statusText}`,
      );
    }

    const body = await response.text();
    if (!body.trim()) {
      return err("The skill file is empty.");
    }

    const { frontmatter, body: parsedBody } = parseFrontmatter(body);
    const name = frontmatter.name || this.skillNameFromUrl(raw);
    const desc = frontmatter.description || `Skill installed from ${url}`;

    // Use createSkill logic
    return this.createSkill(name, desc, parsedBody, scope, repoPath);
  }

  async toggleEnabled(
    id: string,
    enabled: boolean,
  ): Promise<SkillOpResult<void>> {
    try {
      this.db.setEnabled(id, enabled);
      return ok(undefined);
    } catch (e) {
      return err(`Failed to toggle skill: ${(e as Error).message}`);
    }
  }

  async setActivationPrefs(
    id: string,
    prefs: SkillActivationPrefs,
  ): Promise<SkillOpResult<void>> {
    try {
      this.db.setActivationPrefs(id, prefs);
      return ok(undefined);
    } catch (e) {
      return err(
        `Failed to set activation prefs: ${(e as Error).message}`,
      );
    }
  }

  // ── Agent Sync ────────────────────────────────────────────────────

  async syncToAgents(
    skillId: string,
    scope: SkillScope = "user",
    repoPath?: string,
  ): Promise<void> {
    const canonicalDir = this.getSkillCanonicalPath(
      skillId,
      scope,
      repoPath,
    );

    for (const target of agentTargets) {
      try {
        await fs.promises.access(target.configDir);
      } catch {
        // Agent not installed — skip
        continue;
      }

      const targetDir = target.getSkillDir(skillId);
      const parentDir = path.dirname(targetDir);

      try {
        await fs.promises.mkdir(parentDir, { recursive: true });

        // Remove existing entry
        try {
          const stat = await fs.promises.lstat(targetDir);
          if (stat.isSymbolicLink()) {
            await fs.promises.unlink(targetDir);
          } else if (stat.isDirectory()) {
            // Real directory — don't delete, skip with warning
            continue;
          }
        } catch (e) {
          if ((e as NodeJS.ErrnoException).code !== "ENOENT") throw e;
        }

        await fs.promises.symlink(canonicalDir, targetDir, "junction");
      } catch {
        // Silently skip individual agent sync failures
      }
    }
  }

  async unsyncFromAgents(skillId: string): Promise<void> {
    for (const target of agentTargets) {
      const targetDir = target.getSkillDir(skillId);

      try {
        const stat = await fs.promises.lstat(targetDir);
        if (stat.isSymbolicLink()) {
          const linkTarget = await fs.promises.readlink(targetDir);
          const resolved = path.resolve(path.dirname(targetDir), linkTarget);
          if (this.isPathInsideSkillsRoot(resolved)) {
            await fs.promises.unlink(targetDir);
          }
        }
      } catch (e) {
        if ((e as NodeJS.ErrnoException).code !== "ENOENT") {
          // Unexpected error — log and continue
        }
      }
    }
  }

  async resyncAll(): Promise<
    SkillOpResult<{ synced: number; skipped: number; errors: number }>
  > {
    const records = this.db.listSkills();
    let synced = 0;
    let errors = 0;

    for (const record of records) {
      try {
        await this.syncToAgents(
          record.id,
          record.scope,
          record.repoPath ?? undefined,
        );
        synced++;
      } catch {
        errors++;
      }
    }

    return ok({ synced, skipped: 0, errors });
  }

  getDetectedAgents(): DetectedAgent[] {
    return agentTargets.map((target) => {
      let installed = false;
      try {
        fs.accessSync(target.configDir);
        installed = true;
      } catch {
        // Not installed
      }
      return {
        id: target.id,
        name: target.name,
        configDir: target.configDir,
        installed,
      };
    });
  }

  getSkillCanonicalPath(
    id: string,
    scope: SkillScope,
    repoPath?: string,
  ): string {
    if (scope === "repo" && repoPath) {
      return path.join(repoPath, ".crab", "skills", id);
    }
    return path.join(this.userSkillsDir, id);
  }

  // ── Private helpers ───────────────────────────────────────────────

  private hashContent(content: string): string {
    return createHash("sha256").update(content).digest("hex");
  }

  private async computeAgentStatus(
    skillId: string,
    scope: SkillScope,
    repoPath?: string,
  ): Promise<
    Record<string, "synced" | "agent_not_installed" | "error">
  > {
    const canonicalDir = this.getSkillCanonicalPath(skillId, scope, repoPath);
    const status: Record<
      string,
      "synced" | "agent_not_installed" | "error"
    > = {};

    for (const target of agentTargets) {
      try {
        await fs.promises.access(target.configDir);
      } catch {
        status[target.id] = "agent_not_installed";
        continue;
      }

      const targetDir = target.getSkillDir(skillId);
      try {
        const stat = await fs.promises.lstat(targetDir);
        if (stat.isSymbolicLink()) {
          const linkTarget = await fs.promises.readlink(targetDir);
          const resolved = path.resolve(path.dirname(targetDir), linkTarget);
          if (resolved === canonicalDir) {
            status[target.id] = "synced";
          } else {
            status[target.id] = "error";
          }
        } else {
          status[target.id] = "error";
        }
      } catch {
        status[target.id] = "error";
      }
    }

    return status;
  }

  private isPathInsideSkillsRoot(candidatePath: string): boolean {
    const relativePath = path.relative(this.userSkillsDir, candidatePath);
    return (
      !relativePath.startsWith("..") &&
      !path.isAbsolute(relativePath)
    );
  }

  private async reconcile(): Promise<void> {
    // Scan canonical directory and reconcile with DB
    try {
      const entries = await fs.promises.readdir(this.userSkillsDir, {
        withFileTypes: true,
      });
      const diskIds = new Set<string>();

      for (const entry of entries) {
        if (!entry.isDirectory() || entry.name.startsWith(".")) continue;
        diskIds.add(entry.name);

        const existing = this.db.getSkill(entry.name);
        if (!existing) {
          // Add missing DB record from disk
          const skillDir = path.join(this.userSkillsDir, entry.name);
          try {
            const content = await fs.promises.readFile(
              path.join(skillDir, "SKILL.md"),
              "utf-8",
            );
            const { frontmatter, body } = parseFrontmatter(content);
            const now = Date.now();
            const record: SkillRecord = {
              id: entry.name,
              name: frontmatter.name || entry.name,
              description: frontmatter.description || "",
              scope: "user",
              repoPath: null,
              sourceUrl: null,
              installedAt: now,
              updatedAt: now,
              contentHash: this.hashContent(body),
              enabled: true,
              activationPrefs: {
                autoActivate: false,
                restrictToAgents: [],
              },
            };
            this.db.upsertSkill(record);
          } catch {
            // Can't read SKILL.md — skip
          }
        }
      }

      // Remove stale DB records for deleted directories
      const dbIds = this.db.getSkillIdsForScope("user");
      for (const id of dbIds) {
        if (!diskIds.has(id)) {
          this.db.deleteSkill(id);
        }
      }
    } catch {
      // User skills dir doesn't exist yet — that's fine
    }
  }

  private githubUrlToRaw(inputUrl: string): string | null {
    const s = inputUrl.trim();
    if (!s) return null;

    if (s.startsWith("https://raw.githubusercontent.com/")) {
      return s;
    }

    const ghMatch = s.match(
      /^https?:\/\/(?:www\.)?github\.com\/([^/]+)\/([^/]+)\/(blob|tree)\/([^/]+)\/(.+)$/i,
    );
    if (ghMatch) {
      const [, owner, repo, kind, ref, rest] = ghMatch;
      const cleanPath = rest.split("#")[0].split("?")[0];
      const resolved =
        kind === "tree"
          ? cleanPath.replace(/\/+$/, "") + "/SKILL.md"
          : cleanPath;
      return `https://raw.githubusercontent.com/${owner}/${repo}/${ref}/${resolved}`;
    }

    const shorthand = s.match(/^([^/\s]+)\/([^/\s]+)\/(.+)$/);
    if (shorthand) {
      const [, owner, repo, rest] = shorthand;
      return `https://raw.githubusercontent.com/${owner}/${repo}/main/${rest}`;
    }

    return null;
  }

  private skillNameFromUrl(raw: string): string {
    const name = raw.split("/").pop()?.replace(/\.md$/i, "");
    return name || "untitled-skill";
  }
}
