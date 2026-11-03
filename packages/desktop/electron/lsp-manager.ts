import { EventEmitter } from "node:events";
import { LspServer, LspServerConfig, ServerCapabilities } from "./lsp-server";
import { discoverServers, refresh as refreshDiscovery, resolveSpawnSpec, DiscoveredServer } from "./lsp-discovery";
import { lspInstaller } from "./lsp-installer";

/**
 * Status of a language server.
 */
export interface LspServerStatus {
  language: string;
  state: "discovered" | "running" | "error" | "stopped" | "restarting";
  command?: string;
  error?: string;
}

/**
 * Manages all language server instances. Orchestrates lazy start,
 * crash recovery, and message relay.
 *
 * Events:
 * - "message" ({ language, message }) — server → client message
 * - "status" (LspServerStatus) — status change for a server
 */
export class LspManager extends EventEmitter {
  private servers = new Map<string, LspServer>();
  private serverConfigs = new Map<string, LspServerConfig>();
  private crashTimestamps = new Map<string, number[]>();
  private static readonly CRASH_WINDOW_MS = 30_000;
  private static readonly MAX_AUTO_RESTARTS = 1;

  constructor() {
    super();
  }

  /**
   * Start a language server for the given language and workspace root.
   * Checks discovery cache, reads config overrides, spawns LspServer.
   */
  async start(language: string, rootPath: string): Promise<ServerCapabilities> {
    // If already running, return existing capabilities
    const existing = this.servers.get(language);
    if (existing && existing.isRunning()) {
      const caps = existing.getCapabilities();
      if (caps) return caps;
    }

    // Find the server config
    const config = this.resolveConfig(language);
    if (!config) {
      throw new Error(`No language server found for "${language}"`);
    }

    const rootUri = `file://${rootPath}`;
    const server = new LspServer(config, rootUri);

    // Wire up message relay
    server.on("message", (message: object) => {
      this.emit("message", { language, message: JSON.stringify(message) });
    });

    // Wire up crash detection
    server.on("crashed", (info: { code: number | null; signal: string | null; stderr: string[] }) => {
      this.servers.delete(language);
      this.emitStatus(language, "error", config.command, `Exited with code ${info.code}`);
      void this.handleCrash(language, rootPath, info);
    });

    server.on("stderr", (text: string) => {
      // stderr output — log but don't act on it
      process.stderr.write(`[lsp:${language}] ${text}`);
    });

    // Start the server
    const capabilities = await server.start();
    this.servers.set(language, server);
    this.serverConfigs.set(language, config);
    this.emitStatus(language, "running", config.command);

    return capabilities;
  }

  /**
   * Forward a raw JSON string to the appropriate server's stdin
   * with Content-Length framing.
   */
  sendToServer(language: string, message: string): void {
    const server = this.servers.get(language);
    if (!server || !server.isRunning()) {
      throw new Error(`No running server for "${language}"`);
    }
    server.send(message);
  }

  /**
   * Graceful shutdown of all running servers.
   */
  async stopAll(): Promise<void> {
    const shutdowns: Promise<void>[] = [];
    for (const [language, server] of this.servers) {
      shutdowns.push(
        server.shutdown().then(() => {
          this.emitStatus(language, "stopped");
        }).catch(() => {
          this.emitStatus(language, "stopped");
        }),
      );
    }
    await Promise.all(shutdowns);
    this.servers.clear();
  }

  /**
   * Stop a specific language server.
   */
  async stop(language: string): Promise<void> {
    const server = this.servers.get(language);
    if (!server) return;
    await server.shutdown();
    this.servers.delete(language);
    this.emitStatus(language, "stopped");
  }

  /**
   * Return status of all configured servers.
   */
  status(): LspServerStatus[] {
    const discovered = discoverServers();
    const statuses: LspServerStatus[] = [];

    // Include running servers
    for (const [language, server] of this.servers) {
      const config = this.serverConfigs.get(language);
      statuses.push({
        language,
        state: server.isRunning() ? "running" : "error",
        command: config?.command,
      });
    }

    // Include discovered but not running servers
    for (const disc of discovered) {
      if (!this.servers.has(disc.language)) {
        statuses.push({
          language: disc.language,
          state: "discovered",
          command: disc.command,
        });
      }
    }

    return statuses;
  }

  /**
   * Refresh the discovery cache.
   */
  refreshDiscovery(): DiscoveredServer[] {
    return refreshDiscovery();
  }

  // ── Private ────────────────────────────────────────────────────

  /**
   * Resolve the config for a language: check user overrides, then
   * fall back to discovery.
   */
  private resolveConfig(language: string): LspServerConfig | null {
    const discovered = discoverServers();
    const match = discovered.find((s) => s.language === language);
    if (!match) return null;

    const spec = resolveSpawnSpec(match);

    // Bundled servers are launched via Electron's embedded Node runtime
    // using ELECTRON_RUN_AS_NODE=1 — that's how we can ship a .mjs LSP
    // entrypoint without requiring Node on the user's PATH.
    const env: Record<string, string> = {};
    if (match.source === "bundled") {
      env.ELECTRON_RUN_AS_NODE = "1";
    }

    return {
      language,
      command: spec.command,
      args: spec.args,
      env: Object.keys(env).length > 0 ? env : undefined,
    };
  }

  /** Install a language server on demand and refresh discovery. */
  async installLanguage(language: string): Promise<DiscoveredServer> {
    await lspInstaller.install(language);
    refreshDiscovery();
    const discovered = discoverServers();
    const match = discovered.find((s) => s.language === language);
    if (!match) {
      throw new Error(`Installation succeeded but discovery couldn't find "${language}"`);
    }
    this.emitStatus(language, "discovered", match.command);
    return match;
  }

  /** Subscribe to installer progress events. */
  onInstallProgress(listener: (progress: Parameters<typeof lspInstaller.emit>[1]) => void): () => void {
    lspInstaller.on("progress", listener);
    return () => lspInstaller.off("progress", listener);
  }

  /**
   * Handle a server crash: auto-restart once within 30s window,
   * prompt on repeated crashes.
   */
  private async handleCrash(
    language: string,
    rootPath: string,
    _info: { code: number | null; signal: string | null; stderr: string[] },
  ): Promise<void> {
    const now = Date.now();
    const timestamps = this.crashTimestamps.get(language) ?? [];
    timestamps.push(now);

    // Prune timestamps older than the crash window
    const recent = timestamps.filter((ts) => now - ts < LspManager.CRASH_WINDOW_MS);
    this.crashTimestamps.set(language, recent);

    if (recent.length <= LspManager.MAX_AUTO_RESTARTS) {
      // Auto-restart
      this.emitStatus(language, "restarting");
      try {
        await this.start(language, rootPath);
      } catch (err) {
        const msg = err instanceof Error ? err.message : String(err);
        this.emitStatus(language, "error", undefined, msg);
      }
    } else {
      // Too many crashes — emit error status so the renderer can prompt
      this.emitStatus(
        language,
        "error",
        undefined,
        `Server crashed ${recent.length} times in 30s. Manual restart required.`,
      );
    }
  }

  /**
   * Emit a status change event.
   */
  private emitStatus(language: string, state: LspServerStatus["state"], command?: string, error?: string): void {
    const status: LspServerStatus = { language, state, command, error };
    this.emit("status", status);
  }
}
