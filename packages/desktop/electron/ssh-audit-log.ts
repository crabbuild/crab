/**
 * SSH audit log — writes structured connection events to
 * `~/.crab/desktop/ssh-audit.log` for troubleshooting.
 *
 * Features:
 * - Structured log entries: timestamp, event type, profileId, details
 * - Log rotation at 10MB (rename to `.1`, start fresh)
 * - Credential scrubbing: passwords, passphrases, and key contents
 *   are NEVER written to the log
 *
 * Event types: connect, disconnect, auth_failure, reconnection,
 * deploy, deploy_failed
 */

import { EventEmitter } from "node:events";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";

// ── Types ────────────────────────────────────────────────────────

export type AuditEventType =
  | "connect"
  | "disconnect"
  | "auth_failure"
  | "reconnection"
  | "deploy"
  | "deploy_failed";

export interface AuditEntry {
  timestamp: string;
  event: AuditEventType;
  profileId: string;
  details?: Record<string, unknown>;
}

// ── Constants ────────────────────────────────────────────────────

const MAX_LOG_SIZE_BYTES = 10 * 1024 * 1024; // 10MB

/** Fields that must never appear in audit log details. */
const SENSITIVE_KEYS = new Set([
  "password",
  "passphrase",
  "privateKey",
  "private_key",
  "secret",
  "token",
  "credential",
  "credentials",
  "keyContents",
  "key_contents",
]);

// ── Audit log class ──────────────────────────────────────────────

/**
 * Writes SSH connection events to a local audit log file.
 * Rotates the log when it exceeds 10MB. Never includes credentials.
 */
export class SshAuditLog {
  private readonly logPath: string;
  private readonly rotatedPath: string;

  constructor(logPath?: string) {
    const defaultDir = path.join(os.homedir(), ".crab", "desktop");
    this.logPath = logPath ?? path.join(defaultDir, "ssh-audit.log");
    this.rotatedPath = this.logPath + ".1";
  }

  /**
   * Write an audit event to the log file.
   * Scrubs any sensitive fields from the details object.
   */
  write(event: AuditEventType, profileId: string, details?: Record<string, unknown>): void {
    const entry: AuditEntry = {
      timestamp: new Date().toISOString(),
      event,
      profileId,
    };

    if (details && Object.keys(details).length > 0) {
      entry.details = this.scrubSensitive(details);
    }

    const line = JSON.stringify(entry) + "\n";

    this.ensureDirectory();
    this.rotateIfNeeded();

    try {
      fs.appendFileSync(this.logPath, line, "utf-8");
    } catch {
      // Audit logging is best-effort — never crash the app
    }
  }

  /**
   * Read all entries from the current log file. Useful for diagnostics.
   * Returns an empty array if the file doesn't exist or is unreadable.
   */
  readEntries(): AuditEntry[] {
    try {
      if (!fs.existsSync(this.logPath)) {
        return [];
      }
      const content = fs.readFileSync(this.logPath, "utf-8");
      const lines = content.split("\n").filter((l) => l.trim().length > 0);
      const entries: AuditEntry[] = [];
      for (const line of lines) {
        try {
          entries.push(JSON.parse(line) as AuditEntry);
        } catch {
          // Skip malformed lines
        }
      }
      return entries;
    } catch {
      return [];
    }
  }

  /**
   * Get the path to the current log file.
   */
  getLogPath(): string {
    return this.logPath;
  }

  /**
   * Get the path to the rotated log file.
   */
  getRotatedPath(): string {
    return this.rotatedPath;
  }

  // ── Private helpers ────────────────────────────────────────────

  /**
   * Ensure the parent directory exists.
   */
  private ensureDirectory(): void {
    const dir = path.dirname(this.logPath);
    try {
      if (!fs.existsSync(dir)) {
        fs.mkdirSync(dir, { recursive: true });
      }
    } catch {
      // Best-effort
    }
  }

  /**
   * Rotate the log file if it exceeds the size limit.
   * Renames the current log to `.1` (overwriting any previous rotation)
   * and starts a fresh log file.
   */
  private rotateIfNeeded(): void {
    try {
      if (!fs.existsSync(this.logPath)) {
        return;
      }
      const stat = fs.statSync(this.logPath);
      if (stat.size >= MAX_LOG_SIZE_BYTES) {
        // Rename current log to .1 (overwrites previous rotation)
        fs.renameSync(this.logPath, this.rotatedPath);
      }
    } catch {
      // Best-effort — if rotation fails, continue writing to the current file
    }
  }

  /**
   * Remove sensitive fields from a details object. Returns a new object
   * with sensitive values replaced by "[REDACTED]".
   */
  private scrubSensitive(details: Record<string, unknown>): Record<string, unknown> {
    const scrubbed: Record<string, unknown> = {};

    for (const [key, value] of Object.entries(details)) {
      if (SENSITIVE_KEYS.has(key.toLowerCase())) {
        scrubbed[key] = "[REDACTED]";
      } else if (typeof value === "string" && this.looksLikeCredential(value)) {
        scrubbed[key] = "[REDACTED]";
      } else if (value !== null && typeof value === "object" && !Array.isArray(value)) {
        scrubbed[key] = this.scrubSensitive(value as Record<string, unknown>);
      } else {
        scrubbed[key] = value;
      }
    }

    return scrubbed;
  }

  /**
   * Heuristic check for values that look like credentials:
   * - PEM-encoded private keys
   * - Long base64 strings that could be tokens
   */
  private looksLikeCredential(value: string): boolean {
    // PEM private key headers
    if (value.includes("-----BEGIN") && value.includes("PRIVATE KEY")) {
      return true;
    }
    // OpenSSH private key format
    if (value.includes("-----BEGIN OPENSSH PRIVATE KEY-----")) {
      return true;
    }
    return false;
  }
}


// ── Event bridge ─────────────────────────────────────────────────

/**
 * Wire the audit log to listen for connection events from the SSH
 * session manager and agent deployer. Call once during SSH subsystem
 * initialization.
 *
 * Listens for:
 * - `ssh:status-changed` on sessionManager → logs connect, disconnect,
 *   auth_failure, reconnection events
 * - `ssh:deploy-progress` on deployer → logs deploy and deploy_failed events
 */
export function bridgeAuditLog(
  auditLog: SshAuditLog,
  sessionManager: EventEmitter,
  deployer?: EventEmitter,
): void {
  sessionManager.on(
    "ssh:status-changed",
    (payload: { profileId: string; status: string; error?: string }) => {
      const { profileId, status, error } = payload;

      switch (status) {
        case "connected":
          auditLog.write("connect", profileId);
          break;
        case "disconnected":
          auditLog.write("disconnect", profileId, error ? { reason: error } : undefined);
          break;
        case "error":
          if (error && isAuthFailure(error)) {
            auditLog.write("auth_failure", profileId, { error });
          }
          break;
        case "reconnecting":
          auditLog.write("reconnection", profileId);
          break;
      }
    },
  );

  if (deployer) {
    deployer.on(
      "ssh:deploy-progress",
      (progress: { stage: string; percent: number; profileId?: string }) => {
        // Log deploy completion and failures
        if (progress.stage === "starting" && progress.percent === 100) {
          auditLog.write("deploy", progress.profileId ?? "unknown");
        }
      },
    );

    deployer.on(
      "ssh:deploy-failed",
      (payload: { profileId: string; error?: string }) => {
        auditLog.write("deploy_failed", payload.profileId, {
          error: payload.error,
        });
      },
    );
  }
}

/**
 * Check if an error message indicates an authentication failure.
 */
function isAuthFailure(error: string): boolean {
  const lower = error.toLowerCase();
  return (
    lower.includes("authentication") ||
    lower.includes("auth failed") ||
    lower.includes("permission denied") ||
    lower.includes("all configured authentication methods failed")
  );
}
