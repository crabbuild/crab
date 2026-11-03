import { execFile as execFileCb } from "node:child_process";
import * as fs from "node:fs/promises";
import * as path from "node:path";
import { promisify } from "node:util";

import type { SymphonyConfig, SymphonyIssue, SymphonyWorkspace } from "./types";

const execFile = promisify(execFileCb);

export class SymphonyWorkspaceManager {
  constructor(private readonly config: SymphonyConfig) {}

  async createForIssue(issue: SymphonyIssue): Promise<SymphonyWorkspace> {
    const key = sanitizeWorkspaceKey(issue.identifier || issue.id);
    const workspacePath = await this.workspacePathForKey(key);
    await assertPathInsideRoot(this.config.workspace.root, workspacePath);

    let createdNow = false;
    try {
      const stat = await fs.stat(workspacePath);
      if (!stat.isDirectory()) {
        throw new Error(`Workspace path exists but is not a directory: ${workspacePath}`);
      }
    } catch (err) {
      if (!isNodeError(err) || err.code !== "ENOENT") throw err;
      await fs.mkdir(workspacePath, { recursive: true });
      createdNow = true;
    }

    const workspace = { path: workspacePath, key, createdNow };
    if (createdNow && this.config.hooks.afterCreate) {
      await this.runHook("after_create", this.config.hooks.afterCreate, workspace.path, issue);
    }
    return workspace;
  }

  async runBeforeRun(workspacePath: string, issue: SymphonyIssue): Promise<void> {
    if (this.config.hooks.beforeRun) {
      await this.runHook("before_run", this.config.hooks.beforeRun, workspacePath, issue);
    }
  }

  async runAfterRun(workspacePath: string, issue: SymphonyIssue): Promise<void> {
    if (!this.config.hooks.afterRun) return;
    try {
      await this.runHook("after_run", this.config.hooks.afterRun, workspacePath, issue);
    } catch {
      // after_run is best-effort by specification.
    }
  }

  async removeForIssue(identifier: string): Promise<void> {
    const workspacePath = await this.workspacePathForKey(sanitizeWorkspaceKey(identifier));
    await assertPathInsideRoot(this.config.workspace.root, workspacePath);

    try {
      const stat = await fs.stat(workspacePath);
      if (!stat.isDirectory()) return;
    } catch {
      return;
    }

    if (this.config.hooks.beforeRemove) {
      try {
        await this.runHook(
          "before_remove",
          this.config.hooks.beforeRemove,
          workspacePath,
          minimalIssue(identifier),
        );
      } catch {
        // before_remove is best-effort by specification.
      }
    }

    await fs.rm(workspacePath, { recursive: true, force: true });
  }

  async validateWorkspacePath(workspacePath: string): Promise<void> {
    await assertPathInsideRoot(this.config.workspace.root, workspacePath);
  }

  private async workspacePathForKey(key: string): Promise<string> {
    return path.resolve(this.config.workspace.root, key);
  }

  private async runHook(
    name: string,
    script: string,
    workspacePath: string,
    issue: SymphonyIssue,
  ): Promise<void> {
    await this.validateWorkspacePath(workspacePath);
    const env = {
      ...process.env,
      SYMPHONY_ISSUE_ID: issue.id,
      SYMPHONY_ISSUE_IDENTIFIER: issue.identifier,
      SYMPHONY_ISSUE_TITLE: issue.title,
      SYMPHONY_WORKSPACE: workspacePath,
    };

    try {
      await execFile("sh", ["-lc", script], {
        cwd: workspacePath,
        env,
        timeout: this.config.hooks.timeoutMs,
        maxBuffer: 10 * 1024 * 1024,
      });
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      throw new Error(`Workspace hook ${name} failed: ${message}`);
    }
  }
}

export function sanitizeWorkspaceKey(identifier: string): string {
  const sanitized = identifier.replace(/[^A-Za-z0-9._-]/g, "_");
  return sanitized || "issue";
}

export async function assertPathInsideRoot(root: string, candidate: string): Promise<void> {
  const canonicalRoot = await canonicalizeAllowMissing(root);
  const canonicalCandidate = await canonicalizeAllowMissing(candidate);
  const relative = path.relative(canonicalRoot, canonicalCandidate);
  if (relative === "" || (!relative.startsWith("..") && !path.isAbsolute(relative))) {
    return;
  }
  throw new Error(`Refusing workspace path outside root: ${candidate}`);
}

async function canonicalizeAllowMissing(inputPath: string): Promise<string> {
  const absolute = path.resolve(inputPath);
  const parts = absolute.split(path.sep).filter(Boolean);
  const root = path.parse(absolute).root;
  let current = root;
  let index = 0;

  for (; index < parts.length; index++) {
    const next = path.join(current, parts[index]);
    try {
      current = await fs.realpath(next);
    } catch (err) {
      if (!isNodeError(err) || err.code !== "ENOENT") throw err;
      break;
    }
  }

  for (; index < parts.length; index++) {
    current = path.join(current, parts[index]);
  }
  return path.normalize(current);
}

function minimalIssue(identifier: string): SymphonyIssue {
  return {
    id: identifier,
    identifier,
    title: identifier,
    description: null,
    priority: null,
    state: "",
    branchName: null,
    url: null,
    labels: [],
    blockedBy: [],
    createdAt: null,
    updatedAt: null,
  };
}

function isNodeError(err: unknown): err is NodeJS.ErrnoException {
  return err instanceof Error && "code" in err;
}
