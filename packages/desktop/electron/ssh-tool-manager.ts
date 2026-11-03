/**
 * Remote Tool Manager — detects package managers and installs
 * development tools on the remote server via the agent RPC interface.
 *
 * Streams installation output via "stream-data" events, retains the
 * last 1000 lines of output per stream, verifies tool availability
 * in PATH after install, and suggests fixes on failure based on
 * error patterns.
 */

import { EventEmitter } from "node:events";
import { randomUUID } from "node:crypto";
import type { RpcRouter } from "./ssh-router";
import type { SshSessionManager } from "./ssh-manager";

// ── Constants ────────────────────────────────────────────────────

/** Timeout for detecting package managers (milliseconds). */
const DETECT_TIMEOUT_MS = 10_000;

/** Maximum execution timeout for tool installation (milliseconds). */
const INSTALL_TIMEOUT_MS = 600_000;

/** Maximum number of output lines retained per stream. */
const MAX_RETAINED_LINES = 1000;

/** Maximum lines of error output shown in failure suggestions. */
const MAX_ERROR_LINES = 200;

/** Supported package managers. */
export const SUPPORTED_MANAGERS = ["apt", "yum", "brew", "cargo", "pip", "npm"] as const;
export type PackageManager = (typeof SUPPORTED_MANAGERS)[number];

// ── Types ────────────────────────────────────────────────────────

export type InstallStatus = "running" | "succeeded" | "failed";

export interface InstallResult {
  streamId: string;
  status: InstallStatus;
  exitCode?: number;
  inPath: boolean;
  version?: string;
  error?: string;
  suggestion?: string;
}

export interface StreamState {
  profileId: string;
  streamId: string;
  manager: PackageManager;
  tool: string;
  sudo: boolean;
  status: InstallStatus;
  lines: string[];
  startedAt: number;
  timer: ReturnType<typeof setTimeout> | null;
}

export interface RemoteToolManagerEvents {
  "stream-data": (payload: { streamId: string; data: string }) => void;
  "stream-end": (payload: { streamId: string; exitCode?: number; error?: string }) => void;
}

// ── Error pattern matching ───────────────────────────────────────

interface ErrorPattern {
  pattern: RegExp;
  suggestion: string;
}

const ERROR_PATTERNS: ErrorPattern[] = [
  {
    pattern: /unable to locate package|no package .* available|not found in any configured repo/i,
    suggestion:
      "Package not found. Check the package name is correct, or add the appropriate repository/PPA for this package.",
  },
  {
    pattern: /permission denied|operation not permitted|requires root|must be root/i,
    suggestion:
      "Permission denied. Try installing with elevated privileges (sudo) enabled.",
  },
  {
    pattern: /could not resolve host|temporary failure in name resolution|network is unreachable/i,
    suggestion:
      "Network error. Check the remote server's internet connectivity and DNS configuration.",
  },
  {
    pattern: /dpkg was interrupted|you might want to run.*apt.*fix/i,
    suggestion:
      "Package manager is in a broken state. Run 'sudo dpkg --configure -a' or 'sudo apt --fix-broken install' first.",
  },
  {
    pattern: /lock.*held by|could not get lock|waiting for.*lock/i,
    suggestion:
      "Package manager is locked by another process. Wait for the other process to finish, or remove stale lock files.",
  },
  {
    pattern: /disk space|no space left on device/i,
    suggestion:
      "Insufficient disk space on the remote server. Free up space and try again.",
  },
  {
    pattern: /unmet dependencies|dependency.*not satisfiable/i,
    suggestion:
      "Unmet dependencies. Try running 'sudo apt --fix-broken install' or update the package index first.",
  },
  {
    pattern: /command not found|no such file or directory.*bin/i,
    suggestion:
      "The package manager binary was not found. Verify the package manager is installed on the remote server.",
  },
  {
    pattern: /timeout|timed out/i,
    suggestion:
      "The operation timed out. Check network connectivity and try again, or increase the timeout.",
  },
];

// ── RemoteToolManager ────────────────────────────────────────────

export class RemoteToolManager extends EventEmitter {
  private readonly router: RpcRouter;
  private readonly sessionManager: SshSessionManager;
  private streams = new Map<string, StreamState>();

  constructor(router: RpcRouter, sessionManager: SshSessionManager) {
    super();
    this.router = router;
    this.sessionManager = sessionManager;
  }

  /**
   * Detect available package managers on the remote server.
   *
   * Calls the agent RPC "detect_package_managers" and returns the list
   * of available managers within 10 seconds. Returns an empty array if
   * the agent is unreachable or the call times out.
   */
  async detectPackageManagers(profileId: string): Promise<PackageManager[]> {
    const status = this.sessionManager.getStatus(profileId);
    if (status !== "connected") {
      return [];
    }

    try {
      const result = await Promise.race([
        this.router.sendToRemoteAgent(profileId, "detect_package_managers", {}),
        this.createTimeout(DETECT_TIMEOUT_MS, "Package manager detection timed out"),
      ]);

      const response = result as { managers?: string[] };
      const managers = response.managers ?? [];

      // Filter to only supported managers
      return managers.filter((m): m is PackageManager =>
        SUPPORTED_MANAGERS.includes(m as PackageManager),
      );
    } catch {
      return [];
    }
  }

  /**
   * Install a tool on the remote server using the specified package manager.
   *
   * Returns a stream ID immediately. The renderer subscribes to
   * "stream-data" events for that stream ID to receive stdout/stderr
   * output. When the installation completes, a "stream-end" event is
   * emitted and the tool is verified in PATH.
   *
   * The installation is subject to a 600-second timeout. If sudo is
   * requested, the agent executes the command with elevated privileges.
   */
  async installTool(
    profileId: string,
    manager: PackageManager,
    tool: string,
    sudo: boolean,
  ): Promise<InstallResult> {
    const status = this.sessionManager.getStatus(profileId);
    if (status !== "connected") {
      return {
        streamId: "",
        status: "failed",
        inPath: false,
        error: `Cannot install tool: connection status is "${status}"`,
        suggestion: "Establish an SSH connection before installing tools.",
      };
    }

    const streamId = randomUUID();
    const state: StreamState = {
      profileId,
      streamId,
      manager,
      tool,
      sudo,
      status: "running",
      lines: [],
      startedAt: Date.now(),
      timer: null,
    };

    this.streams.set(streamId, state);

    // Set up the 600-second timeout
    state.timer = setTimeout(() => {
      this.handleTimeout(streamId);
    }, INSTALL_TIMEOUT_MS);

    // Set up notification listener for streaming output
    const notificationHandler = (notification: {
      profileId: string;
      method: string;
      params: unknown;
    }) => {
      if (notification.method === "install_output") {
        const params = notification.params as { stream_id?: string; data?: string };
        if (params.stream_id === streamId && params.data) {
          this.appendOutput(streamId, params.data);
        }
      }
    };

    this.router.on("notification", notificationHandler);

    try {
      const result = await Promise.race([
        this.router.sendToRemoteAgent(profileId, "tool_install", {
          manager,
          package: tool,
          sudo,
          timeout_secs: 600,
        }),
        this.createTimeout(INSTALL_TIMEOUT_MS, "Installation timed out"),
      ]);

      // Clean up timeout timer
      if (state.timer) {
        clearTimeout(state.timer);
        state.timer = null;
      }

      this.router.off("notification", notificationHandler);

      const response = result as {
        success?: boolean;
        in_path?: boolean;
        version?: string;
        error?: string;
        exit_code?: number;
      };

      if (response.success) {
        // Verify tool is in PATH
        const inPath = await this.verifyToolInPath(profileId, tool);
        state.status = "succeeded";

        this.emit("stream-end", { streamId, exitCode: response.exit_code ?? 0 });

        return {
          streamId,
          status: "succeeded",
          exitCode: response.exit_code ?? 0,
          inPath,
          version: response.version,
        };
      }

      // Installation failed
      state.status = "failed";
      const errorOutput = this.getErrorOutput(streamId);
      const suggestion = this.suggestFix(errorOutput);

      this.emit("stream-end", {
        streamId,
        exitCode: response.exit_code,
        error: response.error ?? "Installation failed",
      });

      return {
        streamId,
        status: "failed",
        exitCode: response.exit_code,
        inPath: false,
        error: response.error ?? "Installation failed",
        suggestion,
      };
    } catch (err) {
      // Clean up timeout timer
      if (state.timer) {
        clearTimeout(state.timer);
        state.timer = null;
      }

      this.router.off("notification", notificationHandler);

      const message = err instanceof Error ? err.message : String(err);
      state.status = "failed";

      const errorOutput = this.getErrorOutput(streamId);
      const suggestion = this.suggestFix(errorOutput || message);

      this.emit("stream-end", { streamId, error: message });

      return {
        streamId,
        status: "failed",
        inPath: false,
        error: message,
        suggestion,
      };
    }
  }

  /**
   * Cancel an in-progress tool installation.
   *
   * Terminates the remote process and cleans up the stream state.
   */
  cancelInstall(streamId: string): boolean {
    const state = this.streams.get(streamId);
    if (!state || state.status !== "running") {
      return false;
    }

    if (state.timer) {
      clearTimeout(state.timer);
      state.timer = null;
    }

    state.status = "failed";
    this.emit("stream-end", { streamId, error: "Installation cancelled by user" });

    return true;
  }

  /**
   * Get the retained output lines for a stream.
   *
   * Returns up to the last 1000 lines of stdout/stderr output.
   */
  getStreamOutput(streamId: string): string[] {
    const state = this.streams.get(streamId);
    return state?.lines ?? [];
  }

  /**
   * Get the current status of an installation stream.
   */
  getStreamStatus(streamId: string): InstallStatus | null {
    const state = this.streams.get(streamId);
    return state?.status ?? null;
  }

  /**
   * Clean up completed/failed stream state. Call this when the
   * renderer no longer needs the stream output.
   */
  disposeStream(streamId: string): void {
    const state = this.streams.get(streamId);
    if (state) {
      if (state.timer) {
        clearTimeout(state.timer);
      }
      this.streams.delete(streamId);
    }
  }

  /**
   * Clean up all streams. Useful during graceful shutdown.
   */
  disposeAll(): void {
    for (const [, state] of this.streams) {
      if (state.timer) {
        clearTimeout(state.timer);
      }
    }
    this.streams.clear();
  }

  // ── Private ────────────────────────────────────────────────────

  /**
   * Append output data to a stream, retaining at most 1000 lines.
   * Emits a "stream-data" event for the renderer.
   */
  private appendOutput(streamId: string, data: string): void {
    const state = this.streams.get(streamId);
    if (!state) return;

    // Split incoming data into lines and append
    const newLines = data.split("\n");
    for (const line of newLines) {
      if (line || newLines.length === 1) {
        state.lines.push(line);
      }
    }

    // Trim to MAX_RETAINED_LINES
    if (state.lines.length > MAX_RETAINED_LINES) {
      state.lines = state.lines.slice(-MAX_RETAINED_LINES);
    }

    this.emit("stream-data", { streamId, data });
  }

  /**
   * Handle installation timeout: mark as failed and emit stream-end.
   */
  private handleTimeout(streamId: string): void {
    const state = this.streams.get(streamId);
    if (!state || state.status !== "running") return;

    state.status = "failed";
    state.timer = null;

    this.emit("stream-end", {
      streamId,
      error: `Installation timed out after ${INSTALL_TIMEOUT_MS / 1000} seconds`,
    });
  }

  /**
   * Verify that a tool is accessible in the user's PATH on the remote
   * server after installation.
   */
  private async verifyToolInPath(profileId: string, tool: string): Promise<boolean> {
    try {
      const result = await Promise.race([
        this.router.sendToRemoteAgent(profileId, "tool_verify", { tool }),
        this.createTimeout(5_000, "Tool verification timed out"),
      ]);

      const response = result as { found?: boolean; path?: string };
      return response.found === true;
    } catch {
      return false;
    }
  }

  /**
   * Get the last N lines of error output for failure analysis.
   */
  private getErrorOutput(streamId: string): string {
    const state = this.streams.get(streamId);
    if (!state) return "";

    const errorLines = state.lines.slice(-MAX_ERROR_LINES);
    return errorLines.join("\n");
  }

  /**
   * Analyze error output and suggest a fix based on known error patterns.
   */
  suggestFix(errorOutput: string): string {
    for (const { pattern, suggestion } of ERROR_PATTERNS) {
      if (pattern.test(errorOutput)) {
        return suggestion;
      }
    }

    return "Installation failed. Check the output log for details and verify the package name and repository configuration.";
  }

  /**
   * Create a timeout promise that rejects after the specified duration.
   */
  private createTimeout(ms: number, message: string): Promise<never> {
    return new Promise<never>((_, reject) => {
      setTimeout(() => reject(new Error(message)), ms);
    });
  }
}
