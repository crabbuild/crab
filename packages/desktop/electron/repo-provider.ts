/**
 * Unified Repo Provider — transport-agnostic interface for all
 * repository operations.
 *
 * Both LocalProvider and RemoteProvider implement this interface,
 * allowing the IPC handler layer to dispatch calls without knowing
 * whether the repo lives on the local filesystem or a remote host.
 *
 * The renderer's `rpc.*` calling convention is unchanged — the
 * abstraction lives entirely in the main process.
 */

import { EventEmitter } from "node:events";

// ── Capability Set ───────────────────────────────────────────────

/**
 * Feature flags describing what a given repository supports at this
 * moment. Determined at open time and updated on connection state
 * changes. The renderer uses these to enable/disable UI controls
 * instead of checking `is_remote`.
 */
export interface CapabilitySet {
  /** File writing (create, rename, delete, write). */
  canEdit: boolean;
  /** Real-time file change notifications. */
  canWatch: boolean;
  /** Hunk-level git staging. */
  canStageHunks: boolean;
  /** Git worktree management. */
  canWorktree: boolean;
  /** PR/forge operations (depends on forge_provider). */
  canForge: boolean;
  /** Crab hydrate/dehydrate. */
  canHydrate: boolean;
  /** Full-text search. */
  canSearch: boolean;
  /** Command execution (terminals, build/run). */
  canExec: boolean;
  /** Move to trash (local only — remote has no trash). */
  canTrash: boolean;
}

// ── Provider Events ──────────────────────────────────────────────

/**
 * Emitted when a file system change is detected in the watched repo.
 * For local repos, `repoPath` is the absolute filesystem path. For
 * remote repos, `repoPath` is the `repo_id`.
 */
export interface FsChangedEvent {
  type: "fs/changed";
  repoPath: string;
}

/**
 * Emitted when the provider's capabilities change (e.g., connection
 * loss causes canEdit to become false).
 */
export interface CapabilitiesChangedEvent {
  type: "capabilities-changed";
  repoId: string;
  capabilities: CapabilitySet;
}

export type ProviderEvent = FsChangedEvent | CapabilitiesChangedEvent;

// ── File System Operation Types ──────────────────────────────────

export interface FsCreateResult {
  path: string;
  kind: string;
}

export interface FsRenameResult {
  oldPath: string;
  newPath: string;
}

export interface FsDeleteResult {
  deleted: string;
  trash: boolean;
}

export interface FsWriteResult {
  path: string;
  written: number;
}

export interface FsReadResult {
  exists: boolean;
  content: string | null;
}

// ── RepoProvider Interface ───────────────────────────────────────

/**
 * Transport-agnostic interface for all repository operations.
 *
 * The generic `request(method, params)` handles any RPC verb — new
 * verbs work automatically without interface changes. The typed
 * sub-methods cover operations that differ between local and remote
 * (file watching, fs mutations, capabilities).
 */
export interface RepoProvider {
  /**
   * Core dispatch — handles any RPC method. This is the primary
   * entry point for all git and agent operations.
   */
  request(method: string, params: Record<string, unknown>): Promise<unknown>;

  // ── File Watching ────────────────────────────────────────────

  /** Start watching for file changes in the given repo. */
  watchStart(repoId: string): Promise<void>;

  /** Stop watching for file changes in the given repo. */
  watchStop(repoId: string): Promise<void>;

  // ── File System Operations ───────────────────────────────────

  /** Create a file or directory. */
  fsCreate(repoId: string, path: string, kind: "file" | "directory"): Promise<FsCreateResult>;

  /** Rename a file or directory. */
  fsRename(repoId: string, oldPath: string, newPath: string): Promise<FsRenameResult>;

  /** Delete a file or directory. Optionally move to trash (local only). */
  fsDelete(repoId: string, path: string, useTrash?: boolean): Promise<FsDeleteResult>;

  /** Write text content to a file. */
  fsWrite(repoId: string, path: string, content: string): Promise<FsWriteResult>;

  /** Read text content from a file. */
  fsRead(repoId: string, path: string): Promise<FsReadResult>;

  // ── Capabilities ─────────────────────────────────────────────

  /** Query the current capability set for this repo. */
  capabilities(repoId: string): Promise<CapabilitySet>;

  // ── Lifecycle ────────────────────────────────────────────────

  /** Clean up resources (watchers, caches, connections). */
  dispose(): void;
}

// ── Provider Event Emitter ───────────────────────────────────────

/**
 * Typed event emitter for provider events. Providers extend this to
 * emit `fs/changed` and `capabilities-changed` events that the IPC
 * layer forwards to the renderer.
 */
export interface ProviderEventEmitter {
  on(event: "fs/changed", listener: (e: FsChangedEvent) => void): this;
  on(event: "capabilities-changed", listener: (e: CapabilitiesChangedEvent) => void): this;
  off(event: "fs/changed", listener: (e: FsChangedEvent) => void): this;
  off(event: "capabilities-changed", listener: (e: CapabilitiesChangedEvent) => void): this;
  emit(event: "fs/changed", e: FsChangedEvent): boolean;
  emit(event: "capabilities-changed", e: CapabilitiesChangedEvent): boolean;
}

/**
 * Base class for providers that need to emit events. Combines the
 * RepoProvider interface contract with typed event emission.
 */
export abstract class BaseProvider extends EventEmitter implements ProviderEventEmitter {
  abstract request(method: string, params: Record<string, unknown>): Promise<unknown>;
  abstract watchStart(repoId: string): Promise<void>;
  abstract watchStop(repoId: string): Promise<void>;
  abstract fsCreate(repoId: string, path: string, kind: "file" | "directory"): Promise<FsCreateResult>;
  abstract fsRename(repoId: string, oldPath: string, newPath: string): Promise<FsRenameResult>;
  abstract fsDelete(repoId: string, path: string, useTrash?: boolean): Promise<FsDeleteResult>;
  abstract fsWrite(repoId: string, path: string, content: string): Promise<FsWriteResult>;
  abstract fsRead(repoId: string, path: string): Promise<FsReadResult>;
  abstract capabilities(repoId: string): Promise<CapabilitySet>;
  abstract dispose(): void;
}

// ── Utility: Full capability set (local default) ─────────────────

/**
 * Returns a CapabilitySet with all flags set to true. Used as the
 * default for local repos which have full access.
 */
export function fullCapabilities(): CapabilitySet {
  return {
    canEdit: true,
    canWatch: true,
    canStageHunks: true,
    canWorktree: true,
    canForge: true,
    canHydrate: true,
    canSearch: true,
    canExec: true,
    canTrash: true,
  };
}

/**
 * Returns a CapabilitySet with all flags set to false. Used when a
 * remote connection is lost and no operations are available.
 */
export function noCapabilities(): CapabilitySet {
  return {
    canEdit: false,
    canWatch: false,
    canStageHunks: false,
    canWorktree: false,
    canForge: false,
    canHydrate: false,
    canSearch: false,
    canExec: false,
    canTrash: false,
  };
}
