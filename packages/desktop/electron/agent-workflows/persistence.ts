import * as fs from "node:fs/promises";
import * as path from "node:path";
import * as os from "node:os";
import { randomUUID } from "node:crypto";
import log from "electron-log";

import type {
  AgentWorkflowDefinition,
  AgentWorkflowNode,
  AgentWorkflowEvent,
  AgentWorkflowRun,
  AgentWorkflowTrigger,
  AgentWorkflowVersionMeta,
} from "./types";
import { validateAgentWorkflowDefinition } from "./validator";

interface PersistedRunData {
  version: 2;
  runs: AgentWorkflowRun[];
}

interface PersistedTriggerData {
  version: 1;
  triggers: AgentWorkflowTrigger[];
}

const RUN_DATA_VERSION = 2;
const TRIGGER_DATA_VERSION = 1;

export function agentWorkflowDefinitionDir(repoPath: string): string {
  return path.join(repoPath, "agent-workflows");
}

export function agentWorkflowStateDir(): string {
  return path.join(os.homedir(), ".crab", "desktop");
}

function runStorePath(): string {
  return path.join(agentWorkflowStateDir(), "agent-workflow-runs.json");
}

function triggerStorePath(): string {
  return path.join(agentWorkflowStateDir(), "agent-workflow-triggers.json");
}

function eventDir(): string {
  return path.join(agentWorkflowStateDir(), "agent-workflow-events");
}

function versionsDir(definitionId: string): string {
  return path.join(agentWorkflowStateDir(), "agent-workflow-versions", safeFilename(definitionId));
}

function definitionFilePath(repoPath: string, definitionId: string): string {
  return path.join(agentWorkflowDefinitionDir(repoPath), `${safeFilename(definitionId)}.json`);
}

export function safeFilename(value: string): string {
  const safe = value.trim().toLowerCase().replace(/[^a-z0-9._-]+/g, "-").replace(/^-+|-+$/g, "");
  return safe || "workflow";
}

async function atomicWriteJson(filePath: string, data: unknown): Promise<void> {
  const dir = path.dirname(filePath);
  await fs.mkdir(dir, { recursive: true });
  const tmpPath = `${filePath}.tmp.${process.pid}.${Date.now()}`;
  await fs.writeFile(tmpPath, `${JSON.stringify(data, null, 2)}\n`, "utf-8");
  await fs.rename(tmpPath, filePath);
}

async function readJson<T>(filePath: string, options: { backupOnCorrupt: boolean }): Promise<T | null> {
  try {
    const raw = await fs.readFile(filePath, "utf-8");
    return JSON.parse(raw) as T;
  } catch (err: unknown) {
    if (isNodeError(err) && err.code === "ENOENT") return null;
    if (options.backupOnCorrupt) {
      await backupCorruptFile(filePath, err instanceof Error ? err.message : String(err));
    } else {
      log.warn(`[agent-workflows] Failed reading JSON file: ${filePath} (${err instanceof Error ? err.message : String(err)})`);
    }
    return null;
  }
}

async function backupCorruptFile(filePath: string, reason: string): Promise<void> {
  try {
    await fs.rename(filePath, `${filePath}.bak.${Date.now()}`);
    log.warn(`[agent-workflows] Corrupted file moved to .bak: ${filePath} (${reason})`);
  } catch {
    log.warn(`[agent-workflows] Failed reading file: ${filePath} (${reason})`);
  }
}

function isNodeError(err: unknown): err is NodeJS.ErrnoException {
  return typeof err === "object" && err !== null && "code" in err;
}

export class AgentWorkflowDefinitionStore {
  async list(repoPath: string): Promise<AgentWorkflowDefinition[]> {
    const dir = agentWorkflowDefinitionDir(repoPath);
    let entries: string[];
    try {
      entries = await fs.readdir(dir);
    } catch (err: unknown) {
      if (isNodeError(err) && err.code === "ENOENT") return [];
      throw err;
    }

    const definitions: AgentWorkflowDefinition[] = [];
    for (const entry of entries) {
      if (!entry.endsWith(".json")) continue;
      const parsed = await readJson<AgentWorkflowDefinition>(path.join(dir, entry), { backupOnCorrupt: false });
      if (!parsed || parsed.schemaVersion !== 2 || !parsed.id) continue;
      const normalized = normalizeDefinitionForRead(parsed);
      const validation = validateAgentWorkflowDefinition(normalized);
      if (validation.valid) {
        definitions.push(normalized);
      } else {
        log.warn(`[agent-workflows] Invalid workflow definition skipped: ${entry}: ${validation.errors.join("; ")}`);
      }
    }

    return definitions.sort((a, b) => b.updatedAt - a.updatedAt);
  }

  async get(repoPath: string, definitionId: string): Promise<AgentWorkflowDefinition | null> {
    const direct = await readJson<AgentWorkflowDefinition>(definitionFilePath(repoPath, definitionId), { backupOnCorrupt: false });
    if (direct?.id === definitionId) {
      const normalized = normalizeDefinitionForRead(direct);
      const validation = validateAgentWorkflowDefinition(normalized);
      if (!validation.valid) {
        throw new Error(validation.errors.join("\n"));
      }
      return normalized;
    }

    const definitions = await this.list(repoPath);
    return definitions.find((definition) => definition.id === definitionId) ?? null;
  }

  async save(repoPath: string, definition: AgentWorkflowDefinition): Promise<AgentWorkflowDefinition> {
    const now = Date.now();
    const existing = await this.get(repoPath, definition.id).catch(() => null);
    const normalized: AgentWorkflowDefinition = {
      ...definition,
      schemaVersion: 2,
      definitionVersion: existing ? existing.definitionVersion + 1 : Math.max(1, Math.floor(definition.definitionVersion || 1)),
      revisionId: randomUUID(),
      tags: normalizeTags(definition.tags),
      maxParallelNodes: Math.max(1, Math.floor(definition.maxParallelNodes || 1)),
      defaultAgentType: definition.defaultAgentType ?? "codex",
      nodes: (definition.nodes ?? []).map(normalizeNodeForSave),
      edges: definition.edges ?? [],
      layout: definition.layout ?? {},
      createdAt: definition.createdAt || existing?.createdAt || now,
      updatedAt: now,
    };
    const validation = validateAgentWorkflowDefinition(normalized);
    if (!validation.valid) {
      throw new Error(validation.errors.join("\n"));
    }
    await atomicWriteJson(definitionFilePath(repoPath, normalized.id), normalized);
    this.saveVersion(normalized).catch((err: unknown) => {
      log.warn(`[agent-workflows] Failed to save version ${normalized.definitionVersion} for ${normalized.id}: ${err instanceof Error ? err.message : String(err)}`);
    });
    return normalized;
  }

  async saveVersion(definition: AgentWorkflowDefinition): Promise<void> {
    const dir = versionsDir(definition.id);
    await fs.mkdir(dir, { recursive: true });
    const versionFile = path.join(dir, `${definition.definitionVersion}.json`);
    const meta: AgentWorkflowVersionMeta = {
      version: definition.definitionVersion,
      revisionId: definition.revisionId,
      name: definition.name,
      timestamp: definition.updatedAt,
    };
    await atomicWriteJson(versionFile, { meta, definition });
  }

  async listVersions(definitionId: string): Promise<AgentWorkflowVersionMeta[]> {
    const dir = versionsDir(definitionId);
    let entries: string[];
    try {
      entries = await fs.readdir(dir);
    } catch (err: unknown) {
      if (isNodeError(err) && err.code === "ENOENT") return [];
      throw err;
    }
    const versions: AgentWorkflowVersionMeta[] = [];
    for (const entry of entries) {
      const match = entry.match(/^(\d+)\.json$/);
      if (!match) continue;
      const parsed = await readJson<{ meta: AgentWorkflowVersionMeta }>(path.join(dir, entry), { backupOnCorrupt: false });
      if (parsed?.meta) versions.push(parsed.meta);
    }
    return versions.sort((a, b) => b.version - a.version);
  }

  async getVersion(definitionId: string, version: number): Promise<AgentWorkflowDefinition | null> {
    const dir = versionsDir(definitionId);
    const versionFile = path.join(dir, `${version}.json`);
    const parsed = await readJson<{ meta: AgentWorkflowVersionMeta; definition: AgentWorkflowDefinition }>(versionFile, { backupOnCorrupt: false });
    return parsed?.definition ?? null;
  }

  async delete(repoPath: string, definitionId: string): Promise<void> {
    const directPath = definitionFilePath(repoPath, definitionId);
    try {
      await fs.unlink(directPath);
      return;
    } catch (err: unknown) {
      if (!isNodeError(err) || err.code !== "ENOENT") throw err;
    }

    const dir = agentWorkflowDefinitionDir(repoPath);
    const entries = await fs.readdir(dir).catch(() => []);
    for (const entry of entries) {
      if (!entry.endsWith(".json")) continue;
      const fullPath = path.join(dir, entry);
      const parsed = await readJson<AgentWorkflowDefinition>(fullPath, { backupOnCorrupt: false });
      if (parsed?.id === definitionId) {
        await fs.unlink(fullPath);
        return;
      }
    }
  }
}

function normalizeDefinitionForRead(definition: AgentWorkflowDefinition): AgentWorkflowDefinition {
  return {
    ...definition,
    tags: normalizeTags(definition.tags),
    nodes: (definition.nodes ?? []).map(normalizeNodeForSave),
    edges: definition.edges ?? [],
    layout: definition.layout ?? {},
  };
}

function normalizeTags(tags: unknown): string[] | undefined {
  if (!Array.isArray(tags)) return undefined;
  const normalized = tags
    .filter((tag): tag is string => typeof tag === "string")
    .map((tag) => tag.trim())
    .filter(Boolean);
  return normalized.length > 0 ? [...new Set(normalized)] : undefined;
}

function normalizeNodeForSave(node: AgentWorkflowNode): AgentWorkflowNode {
  if (node.type === "loop") {
    return {
      ...node,
      loop: {
        maxIterations: Math.max(1, Math.min(50, Math.floor(node.loop?.maxIterations ?? 1))),
        continueCondition: node.loop?.continueCondition,
        terminateCondition: node.loop?.terminateCondition,
        initialState: isPlainObject(node.loop?.initialState) ? node.loop?.initialState : undefined,
        stateMapping: isPlainObject(node.loop?.stateMapping) ? node.loop?.stateMapping : undefined,
      },
    };
  }
  return node;
}

function isPlainObject(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

export class AgentWorkflowRunStore {
  async list(repoPath?: string): Promise<AgentWorkflowRun[]> {
    const data = await this.readAll();
    const runs = repoPath ? data.runs.filter((run) => run.repoPath === repoPath) : data.runs;
    return runs.sort((a, b) => b.startedAt - a.startedAt);
  }

  async get(runId: string): Promise<AgentWorkflowRun | null> {
    const data = await this.readAll();
    return data.runs.find((run) => run.id === runId) ?? null;
  }

  async save(run: AgentWorkflowRun): Promise<void> {
    const data = await this.readAll();
    const index = data.runs.findIndex((existing) => existing.id === run.id);
    if (index >= 0) {
      data.runs[index] = run;
    } else {
      data.runs.push(run);
    }
    await atomicWriteJson(runStorePath(), data);
  }

  async appendEvent(event: AgentWorkflowEvent): Promise<void> {
    await fs.mkdir(eventDir(), { recursive: true });
    await fs.appendFile(
      path.join(eventDir(), `${safeFilename(event.runId)}.jsonl`),
      `${JSON.stringify(event)}\n`,
      "utf-8",
    );
  }

  async listEvents(runId: string): Promise<AgentWorkflowEvent[]> {
    const filePath = path.join(eventDir(), `${safeFilename(runId)}.jsonl`);
    try {
      const raw = await fs.readFile(filePath, "utf-8");
      return raw
        .split("\n")
        .map((line) => line.trim())
        .filter(Boolean)
        .flatMap((line) => {
          try {
            return [JSON.parse(line) as AgentWorkflowEvent];
          } catch {
            return [];
          }
        });
    } catch (err: unknown) {
      if (isNodeError(err) && err.code === "ENOENT") return [];
      return [];
    }
  }

  private async readAll(): Promise<PersistedRunData> {
    const data = await readJson<PersistedRunData>(runStorePath(), { backupOnCorrupt: true });
    if (!data || data.version !== RUN_DATA_VERSION || !Array.isArray(data.runs)) {
      return { version: RUN_DATA_VERSION, runs: [] };
    }
    return data;
  }
}

export class AgentWorkflowTriggerStore {
  async list(repoPath?: string): Promise<AgentWorkflowTrigger[]> {
    const data = await this.readAll();
    const triggers = repoPath ? data.triggers.filter((trigger) => trigger.repoPath === repoPath) : data.triggers;
    return triggers.sort((a, b) => b.updatedAt - a.updatedAt);
  }

  async get(triggerId: string): Promise<AgentWorkflowTrigger | null> {
    const data = await this.readAll();
    return data.triggers.find((trigger) => trigger.id === triggerId) ?? null;
  }

  async save(trigger: AgentWorkflowTrigger): Promise<AgentWorkflowTrigger> {
    const now = Date.now();
    const normalized: AgentWorkflowTrigger = {
      ...trigger,
      id: trigger.id || randomUUID(),
      kind: trigger.kind || "schedule",
      enabled: trigger.enabled ?? true,
      rrule: trigger.rrule || "",
      webhookToken: trigger.kind === "webhook" ? (trigger.webhookToken || randomUUID().replace(/-/g, "").slice(0, 16)) : undefined,
      createdAt: trigger.createdAt || now,
      updatedAt: now,
    };
    if (!normalized.repoPath) throw new Error("Trigger repoPath is required.");
    if (!normalized.workflowDefinitionId) throw new Error("Trigger workflowDefinitionId is required.");
    if (normalized.kind === "schedule" && !normalized.rrule) throw new Error("Schedule trigger requires an rrule.");

    const data = await this.readAll();
    const index = data.triggers.findIndex((existing) => existing.id === normalized.id);
    if (index >= 0) data.triggers[index] = normalized;
    else data.triggers.push(normalized);
    await atomicWriteJson(triggerStorePath(), data);
    return normalized;
  }

  async delete(triggerId: string): Promise<void> {
    const data = await this.readAll();
    data.triggers = data.triggers.filter((trigger) => trigger.id !== triggerId);
    await atomicWriteJson(triggerStorePath(), data);
  }

  async setEnabled(triggerId: string, enabled: boolean): Promise<AgentWorkflowTrigger> {
    const data = await this.readAll();
    const trigger = data.triggers.find((candidate) => candidate.id === triggerId);
    if (!trigger) throw new Error(`Workflow trigger not found: ${triggerId}.`);
    trigger.enabled = enabled;
    trigger.updatedAt = Date.now();
    await atomicWriteJson(triggerStorePath(), data);
    return trigger;
  }

  async markFired(triggerId: string, scheduledAt: number, runId: string): Promise<void> {
    const data = await this.readAll();
    const trigger = data.triggers.find((candidate) => candidate.id === triggerId);
    if (!trigger) return;
    trigger.lastScheduledAt = scheduledAt;
    trigger.lastRunId = runId;
    trigger.updatedAt = Date.now();
    await atomicWriteJson(triggerStorePath(), data);
  }

  private async readAll(): Promise<PersistedTriggerData> {
    const data = await readJson<PersistedTriggerData>(triggerStorePath(), { backupOnCorrupt: true });
    if (!data || data.version !== TRIGGER_DATA_VERSION || !Array.isArray(data.triggers)) {
      return { version: TRIGGER_DATA_VERSION, triggers: [] };
    }
    return data;
  }
}
