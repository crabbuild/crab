/**
 * Workspace Scripts for the Crab Desktop Orchestrator.
 *
 * Handles setup/run/archive script execution, port allocation,
 * and environment variable injection for orchestrator workspaces.
 */

import { spawn, ChildProcess } from "node:child_process";
import * as fs from "node:fs";
import * as path from "node:path";

// ─── Types ───────────────────────────────────────────────────────────────────

export interface OrchestratorScripts {
  setupScript: string;
  runScript: string;
  archiveScript: string;
}

export interface ScriptResult {
  success: boolean;
  output: string;
  exitCode: number;
  error?: string;
}

export interface WorkspaceEnv {
  CRAB_WORKSPACE_PATH: string;
  CRAB_WORKSPACE_BRANCH: string;
  CRAB_ROOT_PATH: string;
  CRAB_PORT: string;
}

interface OrchestratorJsonConfig {
  scripts?: {
    setup?: string;
    run?: string;
    archive?: string;
  };
  portRange?: {
    start?: number;
    end?: number;
  };
  /** When true, only one workspace run process is allowed at a time. */
  nonconcurrentRun?: boolean;
}

// ─── Port Allocation ─────────────────────────────────────────────────────────

const DEFAULT_PORT_RANGE_START = 3100;
const DEFAULT_PORT_RANGE_END = 3199;

/**
 * Allocates the next available port from the configured range.
 * Returns -1 if no ports are available.
 */
export function allocatePort(
  usedPorts: Set<number>,
  rangeStart = DEFAULT_PORT_RANGE_START,
  rangeEnd = DEFAULT_PORT_RANGE_END,
): number {
  for (let port = rangeStart; port <= rangeEnd; port++) {
    if (!usedPorts.has(port)) {
      return port;
    }
  }
  return -1;
}

// ─── Script Loading ──────────────────────────────────────────────────────────

/**
 * Loads orchestrator scripts from `crab-orchestrator.json` at the repo root
 * if present, otherwise falls back to the provided settings values.
 */
export function loadScripts(
  repoPath: string,
  settingsScripts: OrchestratorScripts,
): OrchestratorScripts {
  const configPath = path.join(repoPath, "crab-orchestrator.json");

  try {
    if (fs.existsSync(configPath)) {
      const raw = fs.readFileSync(configPath, "utf-8");
      const config: OrchestratorJsonConfig = JSON.parse(raw);

      return {
        setupScript: config.scripts?.setup ?? settingsScripts.setupScript,
        runScript: config.scripts?.run ?? settingsScripts.runScript,
        archiveScript: config.scripts?.archive ?? settingsScripts.archiveScript,
      };
    }
  } catch {
    // If the file is malformed, fall back to settings.
  }

  return settingsScripts;
}

/**
 * Loads the nonconcurrent run setting from `crab-orchestrator.json`.
 * When true, only one workspace run process is allowed at a time —
 * starting a run in one workspace stops the active run in another.
 * Defaults to false (concurrent mode).
 */
export function loadNonconcurrentRun(repoPath: string): boolean {
  const configPath = path.join(repoPath, "crab-orchestrator.json");

  try {
    if (fs.existsSync(configPath)) {
      const raw = fs.readFileSync(configPath, "utf-8");
      const config: OrchestratorJsonConfig = JSON.parse(raw);
      return config.nonconcurrentRun ?? false;
    }
  } catch {
    // Fall back to default (concurrent).
  }

  return false;
}

// ─── Environment Variables ───────────────────────────────────────────────────

/**
 * Builds the environment variables injected into workspace scripts.
 */
export function buildWorkspaceEnv(
  worktreePath: string,
  branch: string,
  repoPath: string,
  port: number,
): WorkspaceEnv {
  return {
    CRAB_WORKSPACE_PATH: worktreePath,
    CRAB_WORKSPACE_BRANCH: branch,
    CRAB_ROOT_PATH: repoPath,
    CRAB_PORT: String(port),
  };
}

// ─── Script Execution ────────────────────────────────────────────────────────

/**
 * Executes the setup script in the worktree directory.
 * Returns the output and exit code. Streams output via the onData callback.
 */
export async function runSetupScript(
  script: string,
  worktreePath: string,
  env: WorkspaceEnv,
  onData?: (data: string) => void,
): Promise<ScriptResult> {
  if (!script.trim()) {
    return { success: true, output: "", exitCode: 0 };
  }

  return runScript(script, worktreePath, env, onData);
}

/**
 * Executes the archive script in the worktree directory before removal.
 */
export async function runArchiveScript(
  script: string,
  worktreePath: string,
  env: WorkspaceEnv,
  onData?: (data: string) => void,
): Promise<ScriptResult> {
  if (!script.trim()) {
    return { success: true, output: "", exitCode: 0 };
  }

  return runScript(script, worktreePath, env, onData);
}

/**
 * Spawns the run script in the worktree directory.
 * Returns the child process for streaming output to the terminal.
 */
export function spawnRunScript(
  script: string,
  worktreePath: string,
  env: WorkspaceEnv,
): ChildProcess | null {
  if (!script.trim()) return null;

  const proc = spawn("sh", ["-c", script], {
    cwd: worktreePath,
    env: { ...process.env, ...env },
    stdio: ["pipe", "pipe", "pipe"],
  });

  return proc;
}

// ─── Internal ────────────────────────────────────────────────────────────────

async function runScript(
  script: string,
  cwd: string,
  env: WorkspaceEnv,
  onData?: (data: string) => void,
): Promise<ScriptResult> {
  return new Promise((resolve) => {
    let output = "";

    const proc = spawn("sh", ["-c", script], {
      cwd,
      env: { ...process.env, ...env },
      stdio: ["pipe", "pipe", "pipe"],
    });

    proc.stdout?.on("data", (chunk: Buffer) => {
      const text = chunk.toString();
      output += text;
      onData?.(text);
    });

    proc.stderr?.on("data", (chunk: Buffer) => {
      const text = chunk.toString();
      output += text;
      onData?.(text);
    });

    proc.on("error", (err) => {
      resolve({
        success: false,
        output,
        exitCode: -1,
        error: err.message,
      });
    });

    proc.on("close", (code) => {
      const exitCode = code ?? 0;
      resolve({
        success: exitCode === 0,
        output,
        exitCode,
        error: exitCode !== 0 ? `Script exited with code ${exitCode}` : undefined,
      });
    });
  });
}
