/**
 * Remote Environment Manager — manages environment variables and PATH
 * entries on the remote server via the agent RPC interface.
 *
 * Provides validation for variable names and values, supports persisting
 * changes to shell configuration files with automatic backup and syntax
 * validation. When persist=true, the agent creates a backup of the shell
 * config (retaining 5 most recent), writes the change, and validates
 * with the shell's syntax check command (bash -n / zsh -n).
 */

import { EventEmitter } from "node:events";
import type { RpcRouter } from "./ssh-router";
import type { SshSessionManager } from "./ssh-manager";

// ── Constants ────────────────────────────────────────────────────

/** Maximum number of environment variables returned by env_list. */
const MAX_ENV_VARS = 1000;

/** Maximum number of PATH entries returned by path_list. */
const MAX_PATH_ENTRIES = 200;

/** Minimum length for an environment variable name. */
const ENV_NAME_MIN_LENGTH = 1;

/** Maximum length for an environment variable name. */
const ENV_NAME_MAX_LENGTH = 255;

/** Maximum length for an environment variable value. */
const ENV_VALUE_MAX_LENGTH = 32_768;

/**
 * Regex for valid environment variable names:
 * - Alphanumeric characters and underscores only
 * - Must not start with a digit
 * - Length 1-255 (enforced separately)
 */
const ENV_NAME_PATTERN = /^[A-Za-z_][A-Za-z0-9_]*$/;


// ── Types ────────────────────────────────────────────────────────

export interface EnvListResult {
  variables: Record<string, string>;
  pathEntries: string[];
  shell: string;
  configFiles: string[];
}

export interface EnvSetResult {
  success: boolean;
  backupPath?: string;
  syntaxError?: string;
}

export interface EnvValidationError {
  code: "INVALID_NAME" | "INVALID_VALUE" | "NAME_TOO_SHORT" | "NAME_TOO_LONG" | "VALUE_TOO_LONG";
  message: string;
}

export interface PathUpdateResult {
  success: boolean;
  backupPath?: string;
  syntaxError?: string;
}

export interface RemoteEnvironmentManagerEvents {
  "env-updated": (payload: { profileId: string }) => void;
  "env-syntax-error": (payload: {
    profileId: string;
    configFile: string;
    error: string;
    backupPath: string;
  }) => void;
  error: (payload: { profileId: string; error: string }) => void;
}

// ── RemoteEnvironmentManager ─────────────────────────────────────

export class RemoteEnvironmentManager extends EventEmitter {
  private readonly router: RpcRouter;
  private readonly sessionManager: SshSessionManager;

  constructor(router: RpcRouter, sessionManager: SshSessionManager) {
    super();
    this.router = router;
    this.sessionManager = sessionManager;
  }

  /**
   * List environment variables, PATH entries, and shell config file
   * paths from the remote server.
   *
   * Returns up to 1000 environment variables, up to 200 PATH entries,
   * the detected shell type, and the list of sourced config files.
   */
  async listEnvironment(profileId: string): Promise<EnvListResult> {
    this.assertConnected(profileId);

    try {
      const result = await this.router.sendToRemoteAgent(profileId, "env_list", {});

      const response = result as {
        variables?: Record<string, string>;
        path_entries?: string[];
        shell?: string;
        config_files?: string[];
      };

      const variables = response.variables ?? {};
      const pathEntries = (response.path_entries ?? []).slice(0, MAX_PATH_ENTRIES);
      const shell = response.shell ?? "bash";
      const configFiles = response.config_files ?? [];

      // Limit variables to MAX_ENV_VARS
      const limitedVariables: Record<string, string> = {};
      const keys = Object.keys(variables);
      for (let i = 0; i < Math.min(keys.length, MAX_ENV_VARS); i++) {
        limitedVariables[keys[i]] = variables[keys[i]];
      }

      return {
        variables: limitedVariables,
        pathEntries,
        shell,
        configFiles,
      };
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      this.emit("error", { profileId, error: message });
      throw err;
    }
  }

  /**
   * Set an environment variable on the remote server.
   *
   * Validates the variable name (alphanumeric + underscore, 1-255 chars,
   * no leading digit) and value (max 32,768 chars) before sending to
   * the agent.
   *
   * When persist=true, the agent writes the export to the specified
   * shell config file (or the default config file if not specified),
   * creating a backup first (retaining 5 most recent). After writing,
   * the agent validates the config with the shell's syntax check.
   *
   * If the syntax check fails, returns the error with the backup path
   * so the caller can offer a revert option.
   */
  async setVariable(
    profileId: string,
    name: string,
    value: string,
    persist: boolean,
    configFile?: string,
  ): Promise<EnvSetResult> {
    // Validate name
    const nameError = this.validateName(name);
    if (nameError) {
      throw new Error(nameError.message);
    }

    // Validate value
    const valueError = this.validateValue(value);
    if (valueError) {
      throw new Error(valueError.message);
    }

    this.assertConnected(profileId);

    try {
      const params: Record<string, unknown> = {
        name,
        value,
        persist,
      };

      if (configFile) {
        params.config_file = configFile;
      }

      const result = await this.router.sendToRemoteAgent(profileId, "env_set", params);

      const response = result as {
        success?: boolean;
        backup_path?: string;
        syntax_error?: string;
      };

      if (response.syntax_error) {
        this.emit("env-syntax-error", {
          profileId,
          configFile: configFile ?? "",
          error: response.syntax_error,
          backupPath: response.backup_path ?? "",
        });

        return {
          success: false,
          backupPath: response.backup_path,
          syntaxError: response.syntax_error,
        };
      }

      this.emit("env-updated", { profileId });

      return {
        success: response.success ?? true,
        backupPath: response.backup_path,
      };
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      this.emit("error", { profileId, error: message });
      throw err;
    }
  }

  /**
   * Delete an environment variable from the remote session.
   *
   * Validates the variable name before sending to the agent.
   */
  async deleteVariable(profileId: string, name: string): Promise<void> {
    const nameError = this.validateName(name);
    if (nameError) {
      throw new Error(nameError.message);
    }

    this.assertConnected(profileId);

    try {
      await this.router.sendToRemoteAgent(profileId, "env_delete", { name });
      this.emit("env-updated", { profileId });
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      this.emit("error", { profileId, error: message });
      throw err;
    }
  }

  /**
   * List PATH entries from the remote server as an ordered array.
   *
   * Returns up to 200 PATH entries in their current order.
   */
  async listPath(profileId: string): Promise<string[]> {
    this.assertConnected(profileId);

    try {
      const result = await this.router.sendToRemoteAgent(profileId, "path_list", {});

      const response = result as { entries?: string[] };
      return (response.entries ?? []).slice(0, MAX_PATH_ENTRIES);
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      this.emit("error", { profileId, error: message });
      throw err;
    }
  }

  /**
   * Update the PATH entries on the remote server.
   *
   * Accepts an ordered array of path entries (up to 200). The agent
   * will update the PATH in the current session and optionally persist
   * to the shell config file.
   */
  async updatePath(
    profileId: string,
    entries: string[],
    persist?: boolean,
    configFile?: string,
  ): Promise<PathUpdateResult> {
    if (entries.length > MAX_PATH_ENTRIES) {
      throw new Error(
        `PATH entries exceed maximum of ${MAX_PATH_ENTRIES}. Received ${entries.length} entries.`,
      );
    }

    this.assertConnected(profileId);

    try {
      const params: Record<string, unknown> = {
        entries,
      };

      if (persist !== undefined) {
        params.persist = persist;
      }

      if (configFile) {
        params.config_file = configFile;
      }

      const result = await this.router.sendToRemoteAgent(profileId, "path_update", params);

      const response = result as {
        success?: boolean;
        backup_path?: string;
        syntax_error?: string;
      };

      if (response.syntax_error) {
        this.emit("env-syntax-error", {
          profileId,
          configFile: configFile ?? "",
          error: response.syntax_error,
          backupPath: response.backup_path ?? "",
        });

        return {
          success: false,
          backupPath: response.backup_path,
          syntaxError: response.syntax_error,
        };
      }

      this.emit("env-updated", { profileId });

      return {
        success: response.success ?? true,
        backupPath: response.backup_path,
      };
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      this.emit("error", { profileId, error: message });
      throw err;
    }
  }

  /**
   * Revert a shell config file to a specific backup.
   *
   * Used when a syntax error is detected after a config write. The
   * agent restores the backup and validates the reverted file.
   */
  async revertConfig(profileId: string, backupPath: string, configFile: string): Promise<boolean> {
    this.assertConnected(profileId);

    try {
      const result = await this.router.sendToRemoteAgent(profileId, "env_revert", {
        backup_path: backupPath,
        config_file: configFile,
      });

      const response = result as { success?: boolean };
      return response.success ?? false;
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      this.emit("error", { profileId, error: message });
      throw err;
    }
  }

  /**
   * Validate an environment variable name.
   *
   * Rules:
   * - Must contain only alphanumeric characters and underscores
   * - Must not start with a digit
   * - Must be 1-255 characters long
   *
   * Returns null if valid, or an EnvValidationError if invalid.
   */
  validateName(name: string): EnvValidationError | null {
    if (name.length < ENV_NAME_MIN_LENGTH) {
      return {
        code: "NAME_TOO_SHORT",
        message: `Environment variable name must be at least ${ENV_NAME_MIN_LENGTH} character. Received empty string.`,
      };
    }

    if (name.length > ENV_NAME_MAX_LENGTH) {
      return {
        code: "NAME_TOO_LONG",
        message: `Environment variable name must not exceed ${ENV_NAME_MAX_LENGTH} characters. Received ${name.length} characters.`,
      };
    }

    if (!ENV_NAME_PATTERN.test(name)) {
      if (/^\d/.test(name)) {
        return {
          code: "INVALID_NAME",
          message: `Environment variable name must not start with a digit. Received "${name}".`,
        };
      }
      return {
        code: "INVALID_NAME",
        message: `Environment variable name must contain only alphanumeric characters and underscores (A-Z, a-z, 0-9, _). Received "${name}".`,
      };
    }

    return null;
  }

  /**
   * Validate an environment variable value.
   *
   * Rules:
   * - Must not exceed 32,768 characters
   *
   * Returns null if valid, or an EnvValidationError if invalid.
   */
  validateValue(value: string): EnvValidationError | null {
    if (value.length > ENV_VALUE_MAX_LENGTH) {
      return {
        code: "VALUE_TOO_LONG",
        message: `Environment variable value must not exceed ${ENV_VALUE_MAX_LENGTH} characters. Received ${value.length} characters.`,
      };
    }

    return null;
  }

  // ── Private ────────────────────────────────────────────────────

  /**
   * Assert that the SSH connection for the given profile is active.
   * Throws a descriptive error if not connected.
   */
  private assertConnected(profileId: string): void {
    const status = this.sessionManager.getStatus(profileId);
    if (status !== "connected") {
      throw new Error(
        `Cannot manage remote environment: SSH connection status is "${status}" for profile "${profileId}".`,
      );
    }
  }
}
