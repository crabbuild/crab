/**
 * Workspace awareness for LSP integration.
 *
 * Handles:
 * - Worktree switch: shut down servers for old root, start fresh for new root
 * - workspace/didChangeWorkspaceFolders notification
 * - workspace/configuration request handling
 * - Multi-repo: independent server instances per repo root
 */

import { lspRegistry } from "./lsp-registry";
import { LspConnection } from "./lsp-connection";

// ── Types ───────────────────────────────────────────────────────────

export interface WorkspaceFolder {
  uri: string;
  name: string;
}

export interface LspWorkspaceConfig {
  servers: Record<string, {
    enabled: boolean;
    settings?: Record<string, unknown>;
  }>;
}

// ── Workspace Manager ───────────────────────────────────────────────

class LspWorkspaceManager {
  private currentRootPath: string | null = null;
  private workspaceFolders: WorkspaceFolder[] = [];
  private config: LspWorkspaceConfig = { servers: {} };

  /**
   * Handle a worktree/workspace switch.
   * Shuts down all servers for the old root and prepares for the new root.
   */
  async switchWorkspace(newRootPath: string): Promise<void> {
    if (this.currentRootPath === newRootPath) return;

    const oldRoot = this.currentRootPath;
    this.currentRootPath = newRootPath;

    // Shut down all servers for the old root
    if (oldRoot) {
      await lspRegistry.disposeAll();
    }

    // Update workspace folders
    this.workspaceFolders = [
      { uri: `file://${newRootPath}`, name: pathBasename(newRootPath) },
    ];
  }

  /**
   * Add a workspace folder (multi-root workspace support).
   * Sends workspace/didChangeWorkspaceFolders if the server supports it.
   */
  addWorkspaceFolder(folderPath: string, connection: LspConnection | null): void {
    const folder: WorkspaceFolder = {
      uri: `file://${folderPath}`,
      name: pathBasename(folderPath),
    };

    // Check if already present
    if (this.workspaceFolders.some((f) => f.uri === folder.uri)) return;

    this.workspaceFolders.push(folder);

    // Notify the server if connection is available
    if (connection) {
      this.sendDidChangeWorkspaceFolders(connection, [folder], []);
    }
  }

  /**
   * Remove a workspace folder.
   * Sends workspace/didChangeWorkspaceFolders if the server supports it.
   */
  removeWorkspaceFolder(folderPath: string, connection: LspConnection | null): void {
    const uri = `file://${folderPath}`;
    const idx = this.workspaceFolders.findIndex((f) => f.uri === uri);
    if (idx < 0) return;

    const [removed] = this.workspaceFolders.splice(idx, 1);

    if (connection) {
      this.sendDidChangeWorkspaceFolders(connection, [], [removed]);
    }
  }

  /**
   * Send workspace/didChangeWorkspaceFolders notification.
   * Only call this for servers that advertise workspace folder support.
   */
  sendDidChangeWorkspaceFolders(
    connection: LspConnection,
    added: WorkspaceFolder[],
    removed: WorkspaceFolder[],
  ): void {
    connection.notify("workspace/didChangeWorkspaceFolders", {
      event: { added, removed },
    });
  }

  /**
   * Handle a workspace/configuration request from the server.
   * Returns the settings for the requested sections.
   */
  handleConfigurationRequest(params: ConfigurationParams): unknown[] {
    return params.items.map((item) => {
      // Look up settings for the requested section
      if (item.section) {
        return this.getSettingsForSection(item.section);
      }
      return {};
    });
  }

  /**
   * Update the workspace configuration.
   */
  setConfig(config: LspWorkspaceConfig): void {
    this.config = config;
  }

  /**
   * Get the current root path.
   */
  getRootPath(): string | null {
    return this.currentRootPath;
  }

  /**
   * Get current workspace folders.
   */
  getWorkspaceFolders(): WorkspaceFolder[] {
    return [...this.workspaceFolders];
  }

  private getSettingsForSection(section: string): unknown {
    // Check if the section matches a known server's settings
    for (const [language, serverConfig] of Object.entries(this.config.servers)) {
      if (serverConfig.settings && section.startsWith(language)) {
        return serverConfig.settings[section] ?? serverConfig.settings ?? {};
      }
      // Handle common patterns like "rust-analyzer" for rust
      if (section === "rust-analyzer" && language === "rust") {
        return serverConfig.settings ?? {};
      }
      if (section === "python" && language === "python") {
        return serverConfig.settings ?? {};
      }
      if (section === "gopls" && language === "go") {
        return serverConfig.settings ?? {};
      }
    }
    return {};
  }
}

// ── Configuration request types ─────────────────────────────────────

interface ConfigurationItem {
  scopeUri?: string;
  section?: string;
}

interface ConfigurationParams {
  items: ConfigurationItem[];
}

// ── Multi-repo manager ──────────────────────────────────────────────

/**
 * Manages independent LSP server instances per repository root.
 * Each repo gets its own set of language servers.
 */
class MultiRepoLspManager {
  private managers = new Map<string, LspWorkspaceManager>();

  /**
   * Get or create a workspace manager for a repo root.
   */
  getManager(repoRoot: string): LspWorkspaceManager {
    let manager = this.managers.get(repoRoot);
    if (!manager) {
      manager = new LspWorkspaceManager();
      this.managers.set(repoRoot, manager);
    }
    return manager;
  }

  /**
   * Remove a repo's workspace manager and shut down its servers.
   */
  async removeRepo(repoRoot: string): Promise<void> {
    const manager = this.managers.get(repoRoot);
    if (manager) {
      await lspRegistry.disposeAll();
      this.managers.delete(repoRoot);
    }
  }

  /**
   * Get all active repo roots.
   */
  getActiveRoots(): string[] {
    return Array.from(this.managers.keys());
  }
}

/** Global singleton multi-repo manager. */
export const multiRepoLspManager = new MultiRepoLspManager();

/** Global singleton workspace manager (for single-repo use). */
export const lspWorkspaceManager = new LspWorkspaceManager();

// ── Helpers ─────────────────────────────────────────────────────────

function pathBasename(p: string): string {
  const segments = p.split("/");
  return segments[segments.length - 1] || p;
}

/**
 * Register a workspace/configuration request handler on an LspConnection.
 * This handles server-initiated requests for configuration.
 */
export function registerWorkspaceConfigHandler(connection: LspConnection): () => void {
  return connection.onNotification("workspace/configuration", (params: unknown) => {
    // Note: workspace/configuration is actually a request, not a notification.
    // However, in our architecture the main process handles the request/response
    // correlation. The renderer receives it as a notification-like message and
    // the response is sent back through the connection.
    const configParams = params as ConfigurationParams;
    if (configParams?.items) {
      const result = lspWorkspaceManager.handleConfigurationRequest(configParams);
      // Send the response back. In practice, the main process handles this
      // as a server→client request and we respond via the connection.
      return result;
    }
  });
}
