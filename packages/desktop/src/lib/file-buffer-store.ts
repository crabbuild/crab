import { parseRepoId } from "../../electron/ssh-types";
import { mainProcess } from "../ipc";

export interface FileBufferState {
  content: string | null;
  dirty: boolean;
  saving: boolean;
  available: boolean;
  error: string | null;
}

interface Target { repoId: string; fullPath: string }
interface Binding extends Target { available: boolean; nativeSessionId: string | null }
type Write = (target: Target, content: string, nativeSessionId: string | null) => Promise<void>;
interface Entry {
  state: FileBufferState;
  acknowledged: string | null;
  target?: Target;
  nativeSessionId: string | null;
  runtimeKnown: boolean;
  bindingVersion: number;
  read?: symbol;
  timer?: ReturnType<typeof setTimeout>;
  task?: Promise<void>;
  listeners: Set<() => void>;
}

const EMPTY: FileBufferState = { content: null, dirty: false, saving: false, available: false, error: null };

/** Own one acknowledged working buffer and one writer across all its views. */
export class FileBufferStore {
  private entries = new Map<string, Entry>();
  private listeners = new Set<() => void>();
  private sequence = 0;

  constructor(private write: Write) {}

  snapshot(key: string): FileBufferState { return this.entries.get(key)?.state ?? EMPTY; }
  version = (): number => this.sequence;
  pendingCount(): number { return [...this.entries.values()].filter(entry => entry.state.dirty).length; }

  subscribe = (key: string, listener: () => void): (() => void) => {
    const entry = this.entry(key);
    entry.listeners.add(listener);
    return () => { entry.listeners.delete(listener); this.collect(key, entry); };
  };

  subscribeAll = (listener: () => void): (() => void) => {
    this.listeners.add(listener);
    return () => { this.listeners.delete(listener); };
  };

  bind(key: string, binding: Binding): void {
    const entry = this.entry(key);
    const changed = !entry.target || entry.target.repoId !== binding.repoId || entry.target.fullPath !== binding.fullPath;
    if (changed) {
      entry.target = { repoId: binding.repoId, fullPath: binding.fullPath };
      entry.runtimeKnown = false;
      entry.bindingVersion++;
    }
    if (!entry.runtimeKnown) this.availability(key, entry, binding.available, binding.nativeSessionId);
    if (changed && binding.available && entry.state.dirty) void this.save(key);
  }

  runtime(repoId: string, available: boolean, nativeSessionId: string | null): void {
    for (const [key, entry] of this.entries) {
      if (entry.target?.repoId !== repoId) continue;
      entry.runtimeKnown = true;
      this.availability(key, entry, available, nativeSessionId);
    }
  }

  edit(key: string, content: string): void {
    const entry = this.entry(key);
    if (entry.state.content === content) return;
    entry.read = undefined;
    this.publish(entry, { content, dirty: entry.state.saving || content !== entry.acknowledged, error: null });
    if (entry.timer) clearTimeout(entry.timer);
    if (entry.state.dirty && entry.state.available) entry.timer = setTimeout(() => { entry.timer = undefined; void this.save(key); }, 300);
  }

  beginRead(key: string): symbol | null {
    const entry = this.entry(key);
    if (entry.state.dirty || entry.state.saving) return null;
    const ticket = Symbol(); entry.read = ticket;
    return ticket;
  }

  finishRead(key: string, ticket: symbol, content?: string | null): boolean {
    const entry = this.entries.get(key);
    if (!entry || entry.read !== ticket || entry.state.dirty || entry.state.saving) return false;
    entry.read = undefined;
    if (content !== undefined) {
      entry.acknowledged = content;
      this.publish(entry, { content, dirty: false });
    }
    return true;
  }

  reload(key: string, content: string): void {
    const entry = this.entry(key);
    entry.read = undefined;
    if (entry.timer) { clearTimeout(entry.timer); entry.timer = undefined; }
    // An admitted write cannot be revoked. If Reload races it, drain the
    // selected content afterward so the older save cannot win on disk.
    if (!entry.state.saving) entry.acknowledged = content;
    this.publish(entry, { content, dirty: entry.state.saving, error: null });
  }

  save(key: string): Promise<void> {
    const entry = this.entry(key);
    if (entry.timer) { clearTimeout(entry.timer); entry.timer = undefined; }
    if (entry.task) return entry.task;
    if (!entry.state.dirty || !entry.state.available || !entry.target) return Promise.resolve();
    // Install ownership before notifying views; a subscriber may request a
    // save synchronously, and must join this writer rather than start another.
    const task = Promise.resolve().then(async () => {
      this.publish(entry, { saving: true, error: null });
      while (entry.state.dirty && entry.state.available && entry.state.content !== null && entry.target) {
        const content = entry.state.content, target = entry.target, bindingVersion = entry.bindingVersion, nativeSessionId = entry.nativeSessionId;
        try {
          await this.write(target, content, nativeSessionId);
          entry.acknowledged = content;
          this.publish(entry, { dirty: entry.state.content !== content, error: null });
        } catch (error) {
          if (bindingVersion !== entry.bindingVersion && entry.state.available) continue;
          if (entry.timer) { clearTimeout(entry.timer); entry.timer = undefined; }
          this.publish(entry, { error: error instanceof Error ? error.message : String(error) });
          break;
        }
      }
    }).finally(() => {
      entry.task = undefined;
      this.publish(entry, { saving: false });
      if (entry.state.dirty && entry.state.available && !entry.state.error) void this.save(key);
      this.collect(key, entry);
    });
    entry.task = task;
    return task;
  }

  private entry(key: string): Entry {
    let entry = this.entries.get(key);
    if (!entry) {
      entry = { state: EMPTY, acknowledged: null, nativeSessionId: null, runtimeKnown: false, bindingVersion: 0, listeners: new Set() };
      this.entries.set(key, entry);
    }
    return entry;
  }

  private availability(key: string, entry: Entry, available: boolean, nativeSessionId: string | null): void {
    const changed = entry.state.available !== available || entry.nativeSessionId !== nativeSessionId;
    if (!changed) return;
    // Local filesystem writes survive native reader replacement. Only SSH
    // writes belong to that session; a local permission failure must stay visible.
    const writeChanged = entry.state.available !== available || !!parseRepoId(entry.target?.repoId ?? "");
    if (writeChanged) entry.bindingVersion++;
    entry.read = undefined; entry.nativeSessionId = nativeSessionId;
    this.publish(entry, { available });
    if (writeChanged && available && entry.state.dirty) void this.save(key);
  }

  private publish(entry: Entry, patch: Partial<FileBufferState>): void {
    entry.state = { ...entry.state, ...patch };
    this.sequence++;
    for (const listener of entry.listeners) listener();
    for (const listener of this.listeners) listener();
  }

  private collect(key: string, entry: Entry): void {
    if (this.entries.get(key) !== entry || entry.listeners.size || entry.state.dirty || entry.task) return;
    if (entry.timer) clearTimeout(entry.timer);
    this.entries.delete(key);
  }
}

export const fileBuffers = new FileBufferStore(async (target, content, nativeSessionId) => {
  const result = await mainProcess.fsWriteFile(target.repoId, target.fullPath, content, nativeSessionId);
  if (result.written !== new TextEncoder().encode(content).length) throw new Error("The file save did not write the complete buffer.");
});
