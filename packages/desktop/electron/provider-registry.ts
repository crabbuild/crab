/**
 * Provider Registry — resolves repo_id values to their corresponding
 * RepoProvider instance (local or remote).
 *
 * The registry is a singleton in the main process. It maps repo_id
 * strings to provider instances and resolves by repo_id:
 *
 * - `ssh://<profileId>/<path>` → requires an opened remote provider
 * - anything else → falls back to the LocalProvider singleton
 *
 * Emits lifecycle events (`provider:registered`, `provider:unregistered`)
 * so other subsystems can react to provider changes.
 */

import { EventEmitter } from "node:events";
import { parseRepoId } from "./ssh-router";
import type { RepoProvider } from "./repo-provider";

// ── Registry Events ──────────────────────────────────────────────

export interface ProviderRegisteredEvent {
  repoId: string;
  provider: RepoProvider;
}

export interface ProviderUnregisteredEvent {
  repoId: string;
}

// ── ProviderRegistry ─────────────────────────────────────────────

export class ProviderRegistry extends EventEmitter {
  private providers = new Map<string, RepoProvider>();
  private localProvider: RepoProvider | null = null;

  /**
   * Set the LocalProvider singleton used as the fallback for any
   * repo_id that doesn't have an explicit registration and doesn't
   * match the `ssh://` prefix.
   */
  setLocalProvider(provider: RepoProvider): void {
    this.localProvider = provider;
  }

  /**
   * Register a provider for a specific repo_id. Overwrites any
   * existing registration for the same repo_id.
   */
  register(repoId: string, provider: RepoProvider): void {
    this.providers.set(repoId, provider);
    this.emit("provider:registered", { repoId, provider } satisfies ProviderRegisteredEvent);
  }

  /**
   * Remove the provider registration for a repo_id. Callers are
   * responsible for disposing the provider if needed.
   */
  unregister(repoId: string): void {
    const existed = this.providers.delete(repoId);
    if (existed) {
      this.emit("provider:unregistered", { repoId } satisfies ProviderUnregisteredEvent);
    }
  }

  /**
   * Resolve the correct provider for a given repo_id.
   *
   * Resolution order:
   * 1. Explicit registration (exact match on repo_id)
   * 2. An unregistered SSH repository throws before any local handling.
   * 3. Otherwise, returns the LocalProvider singleton
   *
   * Returns `null` if the local provider has not initialized.
   */
  resolve(repoId: string): RepoProvider | null {
    // 1. Check explicit registrations
    const registered = this.providers.get(repoId);
    if (registered) return registered;

    if (parseRepoId(repoId)) {
      // Every IPC surface resolves here. A missing SSH registration must never
      // turn a remote operation into a local file write, watcher or RPC.
      throw Object.assign(new Error("SSH repository is not open. Reopen it from SSH Connections."), { code: "SSH_REPOSITORY_NOT_OPEN" });
    }

    // 3. Local repo_id — fall back to LocalProvider singleton
    return this.localProvider;
  }

  /**
   * Check whether a provider is registered for the given repo_id.
   */
  has(repoId: string): boolean {
    return this.providers.has(repoId);
  }

  /**
   * Get all currently registered repo_ids.
   */
  registeredIds(): string[] {
    return [...this.providers.keys()];
  }

  /**
   * Remove all registrations and reset the registry. Does NOT
   * dispose providers — callers must handle cleanup.
   */
  clear(): void {
    for (const repoId of this.providers.keys()) {
      this.emit("provider:unregistered", { repoId } satisfies ProviderUnregisteredEvent);
    }
    this.providers.clear();
  }
}

// ── Singleton ────────────────────────────────────────────────────

let _instance: ProviderRegistry | null = null;

/**
 * Get the global ProviderRegistry singleton. Created on first access.
 */
export function getProviderRegistry(): ProviderRegistry {
  if (!_instance) {
    _instance = new ProviderRegistry();
  }
  return _instance;
}

/**
 * Reset the singleton (for testing only).
 */
export function _resetProviderRegistry(): void {
  _instance = null;
}
