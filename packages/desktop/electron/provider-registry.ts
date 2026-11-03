/**
 * Provider Registry — resolves repo_id values to their corresponding
 * RepoProvider instance (local or remote).
 *
 * The registry is a singleton in the main process. It maps repo_id
 * strings to provider instances and provides automatic fallback
 * resolution based on the repo_id format:
 *
 * - `ssh://<profileId>/<path>` → attempts RpcRouter-based resolution
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
   * 2. If repo_id has `ssh://` prefix and no explicit registration,
   *    returns `null` — the caller should route through RpcRouter
   *    directly as a best-effort fallback (per Requirement 14.2)
   * 3. Otherwise, returns the LocalProvider singleton
   *
   * This method never throws. If no provider can be resolved (e.g.,
   * LocalProvider hasn't been set yet), it returns `null`.
   */
  resolve(repoId: string): RepoProvider | null {
    // 1. Check explicit registrations
    const registered = this.providers.get(repoId);
    if (registered) return registered;

    // 2. Check if this is a remote repo_id with no registration
    const parsed = parseRepoId(repoId);
    if (parsed) {
      // No explicit registration for this remote repo — return null
      // so the caller can attempt RpcRouter-based routing directly.
      return null;
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
