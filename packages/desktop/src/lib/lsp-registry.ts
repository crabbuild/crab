/**
 * Singleton LSP registry that manages active LspClient instances per language.
 *
 * Deduplicates concurrent start requests and provides a React hook
 * for components to access LSP clients.
 */

import { useEffect, useState } from "react";
import { LspClient } from "./lsp-client";
import type { LspServerCapabilities } from "./lsp-types";

class LspRegistry {
  private clients = new Map<string, LspClient>();
  private starting = new Map<string, Promise<LspClient | null>>();

  /**
   * Get or start an LSP client for a language.
   * Returns null if the server is not available.
   * Deduplicates concurrent start requests for the same language.
   */
  async getClient(language: string, rootPath: string): Promise<LspClient | null> {
    // Return existing client
    const existing = this.clients.get(language);
    if (existing) return existing;

    // Return in-flight start promise (deduplication)
    const inflight = this.starting.get(language);
    if (inflight) return inflight;

    // Start a new client
    const promise = this.startClient(language, rootPath);
    this.starting.set(language, promise);

    try {
      const client = await promise;
      this.starting.delete(language);
      if (client) {
        this.clients.set(language, client);
      }
      return client;
    } catch {
      this.starting.delete(language);
      return null;
    }
  }

  /**
   * Check if a client is already running for a language (synchronous).
   */
  hasClient(language: string): boolean {
    return this.clients.has(language);
  }

  /**
   * Get an existing client synchronously (returns undefined if not started).
   */
  getExistingClient(language: string): LspClient | undefined {
    return this.clients.get(language);
  }

  /**
   * Get all currently-running clients, paired with their language id.
   * Useful for features that query multiple servers (workspace symbols,
   * health view, diagnostics bundles).
   */
  getAllClients(): Array<{ language: string; client: LspClient }> {
    return Array.from(this.clients.entries()).map(([language, client]) => ({ language, client }));
  }

  /**
   * Shut down all clients. Called on workspace change or app close.
   */
  async disposeAll(): Promise<void> {
    // Wait for any in-flight starts to complete
    const inflight = Array.from(this.starting.values());
    await Promise.allSettled(inflight);

    // Dispose all clients
    for (const [language, client] of this.clients) {
      try {
        client.dispose();
        await window.crab.lsp.stop(language);
      } catch {
        // Best-effort cleanup
      }
    }
    this.clients.clear();
    this.starting.clear();
  }

  /**
   * Dispose a single client by language.
   */
  async disposeClient(language: string): Promise<void> {
    const client = this.clients.get(language);
    if (client) {
      client.dispose();
      this.clients.delete(language);
      try {
        await window.crab.lsp.stop(language);
      } catch {
        // Best-effort
      }
    }
  }

  private async startClient(language: string, rootPath: string): Promise<LspClient | null> {
    try {
      const capabilities = await window.crab.lsp.start(language, rootPath) as LspServerCapabilities;
      return new LspClient(language, capabilities);
    } catch {
      // Server not available or failed to start
      return null;
    }
  }
}

/** Global singleton registry instance. */
export const lspRegistry = new LspRegistry();

// ── React Hook ──────────────────────────────────────────────────────

/**
 * React hook that provides an LSP client for a given language and root path.
 * Returns null while the client is initializing or if no server is available.
 */
export function useLspClient(language: string | null, rootPath: string | null): LspClient | null {
  const [client, setClient] = useState<LspClient | null>(null);

  useEffect(() => {
    if (!language || !rootPath || language === "plaintext") {
      setClient(null);
      return;
    }

    let cancelled = false;

    // Check for existing client first (synchronous fast path)
    const existing = lspRegistry.getExistingClient(language);
    if (existing) {
      setClient(existing);
      return;
    }

    // Start client asynchronously
    lspRegistry.getClient(language, rootPath).then((c) => {
      if (!cancelled) setClient(c);
    });

    return () => {
      cancelled = true;
    };
  }, [language, rootPath]);

  return client;
}
