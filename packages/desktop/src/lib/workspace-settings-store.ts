import type { Settings } from "../settings";
import { mainProcess } from "../ipc";

export interface TaskItem {
  id: string;
  title: string;
  done: boolean;
  anchor?: string;
  createdAt: number;
}

const boolean = (value: unknown) => typeof value === "boolean";
const string = (value: unknown) => typeof value === "string";
const number = (min: number, max: number) => (value: unknown) => typeof value === "number" && Number.isFinite(value) && value >= min && value <= max;
const oneOf = (...values: unknown[]) => (value: unknown) => values.includes(value);

// Repository files may configure presentation and repository workflows, never
// device paths, credentials, agent execution, telemetry or global services.
const RULES = {
  theme: oneOf("dark", "light", "hc-dark", "hc-light", "system"),
  editorFontSize: number(10, 20), editorWordWrap: boolean, editorMinimap: boolean,
  editorThemeDark: string, editorThemeLight: string, showActivityBar: boolean,
  maxPreviewKib: number(64, 16384), parquetPreviewRows: oneOf(50, 100, 200, 500),
  preferCrabDiff: boolean, discardConfirmThreshold: number(0, 10000),
  conventionalCommits: boolean, stripNotebookOutputs: boolean, largeFileThresholdMb: number(0, Number.MAX_SAFE_INTEGER),
} satisfies Partial<Record<keyof Settings, (value: unknown) => boolean>>;

export function isWorkspaceSetting(key: string): key is keyof typeof RULES { return Object.hasOwn(RULES, key); }

export function workspaceOverrides(document: Record<string, unknown> | null): Partial<Settings> {
  return Object.fromEntries(Object.entries(document ?? {}).filter(([key]) => isWorkspaceSetting(key))) as Partial<Settings>;
}

export function documentTasks(document: Record<string, unknown> | null): TaskItem[] {
  return (document?.tasks ?? []) as TaskItem[];
}

function validate(document: unknown): asserts document is Record<string, unknown> {
  if (!document || typeof document !== "object" || Array.isArray(document)) throw new Error("Workspace settings must contain a JSON object.");
  for (const [key, value] of Object.entries(document)) {
    if (isWorkspaceSetting(key) && !RULES[key](value)) throw new Error(`Invalid workspace setting: ${key}.`);
    if (key === "tasks" && (!Array.isArray(value) || value.some(task => !task || typeof task !== "object"
      || typeof task.id !== "string" || typeof task.title !== "string" || typeof task.done !== "boolean"
      || typeof task.createdAt !== "number" || !Number.isFinite(task.createdAt) || (task.anchor !== undefined && typeof task.anchor !== "string")))) {
      throw new Error("Workspace tasks contain an invalid entry.");
    }
  }
}

export interface WorkspaceDocumentState {
  document: Record<string, unknown> | null;
  loading: boolean;
  saving: boolean;
  error: string | null;
}

interface Target { repoId: string; fullPath: string; canEdit: boolean; owner: string | null }
interface Entry {
  target?: Target;
  state: WorkspaceDocumentState;
  tail: Promise<void>;
  read?: Promise<boolean>;
  listeners: Set<() => void>;
}
type Read = (target: Target) => Promise<{ exists: boolean; content: string | null }>;
type Write = (target: Target, content: string) => Promise<number>;
const EMPTY: WorkspaceDocumentState = { document: null, loading: false, saving: false, error: null };
export const WORKSPACE_DOCUMENT_LIMIT = 64 * 1024;

/** Serialize complete read-modify-write operations shared by all workspace views. */
export class WorkspaceSettingsStore {
  private entries = new Map<string, Entry>();
  private mutations = 0;
  pendingCount(): number { return this.mutations; }
  constructor(private read: Read, private write: Write) {}
  snapshot(key: string): WorkspaceDocumentState { return this.entries.get(key)?.state ?? EMPTY; }
  subscribe(key: string, listener: () => void): () => void {
    const entry = this.entry(key); entry.listeners.add(listener);
    return () => { entry.listeners.delete(listener); };
  }
  bind(key: string, target: Target): void {
    const entry = this.entry(key), old = entry.target;
    if (!old || old.repoId !== target.repoId || old.fullPath !== target.fullPath || old.canEdit !== target.canEdit || old.owner !== target.owner) { entry.target = target; entry.read = undefined; }
  }
  load(key: string): Promise<boolean> {
    const entry = this.entry(key);
    if (entry.read) return entry.read;
    const read = this.run(key).finally(() => { if (entry.read === read) entry.read = undefined; });
    entry.read = read;
    return read;
  }
  update(key: string, change: (document: Record<string, unknown>) => void): Promise<boolean> {
    this.mutations++;
    return this.run(key, change).finally(() => { this.mutations--; });
  }
  private run(key: string, change?: (document: Record<string, unknown>) => void): Promise<boolean> {
    const entry = this.entry(key);
    // Every mutation reads after the preceding acknowledgement, including
    // mutations of the same tasks array. A stale mounted hook cannot win.
    const work = entry.tail.then(async () => {
      const target = entry.target;
      this.publish(entry, { loading: !change, saving: !!change, error: null });
      try {
        if (!target || (change && !target.canEdit)) throw new Error("This repository has no writable workspace.");
        const result = await this.read(target);
        if (result.exists && result.content === null) throw new Error("Workspace settings is not a readable text file.");
        if (result.content !== null && new TextEncoder().encode(result.content).length > WORKSPACE_DOCUMENT_LIMIT) throw new Error("Workspace settings exceeds the 64 KiB document limit.");
        const document: unknown = result.exists ? JSON.parse(result.content!) : {};
        validate(document);
        if (entry.target !== target) throw new Error("The repository changed while reading workspace settings. Retry the action.");
        if (change) {
          change(document); validate(document);
          const content = JSON.stringify(document, null, 2) + "\n", bytes = new TextEncoder().encode(content).length;
          if (bytes > WORKSPACE_DOCUMENT_LIMIT) throw new Error("Workspace settings exceeds the 64 KiB document limit.");
          if (await this.write(target, content) !== bytes) throw new Error("Workspace settings was not saved completely.");
        }
        if (entry.target === target) this.publish(entry, { document });
        return true;
      } catch (error) {
        this.publish(entry, { error: error instanceof Error ? error.message : String(error) });
        return false;
      } finally { this.publish(entry, { loading: false, saving: false }); }
    });
    entry.tail = work.then(() => {});
    return work;
  }
  private entry(key: string): Entry {
    let entry = this.entries.get(key);
    if (!entry) { entry = { state: EMPTY, tail: Promise.resolve(), listeners: new Set() }; this.entries.set(key, entry); }
    return entry;
  }
  private publish(entry: Entry, state: Partial<WorkspaceDocumentState>): void {
    entry.state = { ...entry.state, ...state };
    for (const listener of entry.listeners) listener();
  }
}

export const workspaceDocuments = new WorkspaceSettingsStore(
  target => mainProcess.fsReadFile(target.repoId, target.fullPath, WORKSPACE_DOCUMENT_LIMIT, target.owner),
  async (target, content) => (await mainProcess.fsWriteFile(target.repoId, target.fullPath, content, target.owner)).written,
);
