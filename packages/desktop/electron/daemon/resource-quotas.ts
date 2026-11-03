/**
 * Resource Quotas — configurable limits for the PTY daemon.
 *
 * Quota configuration lives at ~/.crab/desktop/daemon/quotas.json.
 * Supported quotas:
 * - maxConcurrentSessions (default: 50) — enforced in TerminalHost.createOrAttach
 * - maxMemoryMb (default: 1024) — when exceeded, refuse new sessions
 * - maxSessionLifetimeHours (default: 0) — disabled by default; positive values kill older sessions
 *
 * Quotas are reloaded on SIGHUP, allowing live reconfiguration without
 * daemon restart. If the config file is missing or malformed, defaults apply.
 */

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const DEFAULT_MAX_CONCURRENT_SESSIONS = 50;
const DEFAULT_MAX_MEMORY_MB = 1024;
const DEFAULT_MAX_SESSION_LIFETIME_HOURS = 0;

/** How often the lifetime enforcement timer fires (60 seconds). */
const LIFETIME_CHECK_INTERVAL_MS = 60_000;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

export interface QuotaConfig {
  maxConcurrentSessions: number;
  maxMemoryMb: number;
  maxSessionLifetimeHours: number;
}

/** A session that the quota manager can inspect and kill. */
export interface QuotaManagedSession {
  readonly sessionId: string;
  readonly isAlive: boolean;
  readonly createdAt: number;
  kill(): void;
}

export type GetQuotaSessionsFn = () => QuotaManagedSession[];

export interface ResourceQuotaOptions {
  /** Path to the quotas.json config file. */
  configPath: string;
  /** Interval for lifetime enforcement checks (ms). Default: 60_000. */
  lifetimeCheckIntervalMs?: number;
  /** Function to read a file. Default: fs.readFileSync. */
  readFile?: (filePath: string) => string;
  /** Function to check if a file exists. Default: fs.existsSync. */
  fileExists?: (filePath: string) => boolean;
  /** Function to get current heap usage in bytes. Default: process.memoryUsage().heapUsed. */
  getHeapUsed?: () => number;
  /** Logger for warnings and info. */
  onWarning?: (message: string, meta?: Record<string, unknown>) => void;
  /** Logger for info messages. */
  onInfo?: (message: string, meta?: Record<string, unknown>) => void;
  /** Clock function for testability. Returns current time in ms. */
  now?: () => number;
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

export class QuotaExceededError extends Error {
  constructor(quota: string, message: string) {
    super(`quota-exceeded:${quota}: ${message}`);
    this.name = "QuotaExceededError";
  }
}

// ---------------------------------------------------------------------------
// ResourceQuotaManager
// ---------------------------------------------------------------------------

export class ResourceQuotaManager {
  private config: QuotaConfig;
  private readonly configPath: string;
  private readonly lifetimeCheckIntervalMs: number;
  private readonly readFile: (filePath: string) => string;
  private readonly fileExists: (filePath: string) => boolean;
  private readonly getHeapUsed: () => number;
  private readonly onWarning: (message: string, meta?: Record<string, unknown>) => void;
  private readonly onInfo: (message: string, meta?: Record<string, unknown>) => void;
  private readonly now: () => number;
  private readonly getSessions: GetQuotaSessionsFn;

  private lifetimeTimer: ReturnType<typeof setInterval> | null = null;
  private sighupHandler: (() => void) | null = null;
  private disposed = false;

  constructor(getSessions: GetQuotaSessionsFn, opts: ResourceQuotaOptions) {
    this.getSessions = getSessions;
    this.configPath = opts.configPath;
    this.lifetimeCheckIntervalMs = opts.lifetimeCheckIntervalMs ?? LIFETIME_CHECK_INTERVAL_MS;
    this.readFile = opts.readFile ?? defaultReadFile;
    this.fileExists = opts.fileExists ?? defaultFileExists;
    this.getHeapUsed = opts.getHeapUsed ?? (() => process.memoryUsage().heapUsed);
    this.onWarning = opts.onWarning ?? (() => {});
    this.onInfo = opts.onInfo ?? (() => {});
    this.now = opts.now ?? (() => Date.now());

    // Load initial config
    this.config = this.loadConfig();
  }

  /** Current quota configuration (read-only snapshot). */
  get quotas(): Readonly<QuotaConfig> {
    return this.config;
  }

  /**
   * Start lifetime enforcement timer and register SIGHUP handler.
   */
  start(): void {
    if (this.disposed) return;

    // Start lifetime enforcement timer
    if (this.lifetimeTimer === null) {
      this.lifetimeTimer = setInterval(
        () => this.enforceLifetime(),
        this.lifetimeCheckIntervalMs,
      );
      if (this.lifetimeTimer.unref) {
        this.lifetimeTimer.unref();
      }
    }

    // Register SIGHUP handler for config reload
    this.sighupHandler = () => this.reload();
    process.on("SIGHUP", this.sighupHandler);
  }

  /**
   * Stop the timer and unregister signal handler.
   */
  dispose(): void {
    if (this.disposed) return;
    this.disposed = true;

    if (this.lifetimeTimer !== null) {
      clearInterval(this.lifetimeTimer);
      this.lifetimeTimer = null;
    }

    if (this.sighupHandler !== null) {
      process.removeListener("SIGHUP", this.sighupHandler);
      this.sighupHandler = null;
    }
  }

  /**
   * Reload quotas from the config file. Called on SIGHUP.
   */
  reload(): void {
    if (this.disposed) return;

    const newConfig = this.loadConfig();
    const oldConfig = this.config;
    this.config = newConfig;

    // Log changes
    if (
      oldConfig.maxConcurrentSessions !== newConfig.maxConcurrentSessions ||
      oldConfig.maxMemoryMb !== newConfig.maxMemoryMb ||
      oldConfig.maxSessionLifetimeHours !== newConfig.maxSessionLifetimeHours
    ) {
      this.onInfo("Quotas reloaded", {
        maxConcurrentSessions: newConfig.maxConcurrentSessions,
        maxMemoryMb: newConfig.maxMemoryMb,
        maxSessionLifetimeHours: newConfig.maxSessionLifetimeHours,
      });
    }
  }

  /**
   * Check whether a new session can be created given current quotas.
   * Throws QuotaExceededError if any quota would be violated.
   *
   * Call this from TerminalHost.createOrAttach before spawning.
   */
  checkCanCreateSession(currentSessionCount: number): void {
    if (this.disposed) return;

    // Check concurrent session limit
    if (currentSessionCount >= this.config.maxConcurrentSessions) {
      throw new QuotaExceededError(
        "maxConcurrentSessions",
        `Maximum concurrent sessions reached: ${this.config.maxConcurrentSessions}`,
      );
    }

    // Check memory limit
    const heapUsedMb = this.getHeapUsed() / (1024 * 1024);
    if (heapUsedMb >= this.config.maxMemoryMb) {
      this.onWarning("Memory quota exceeded, refusing new session", {
        heapUsedMb: Math.round(heapUsedMb),
        maxMemoryMb: this.config.maxMemoryMb,
      });
      throw new QuotaExceededError(
        "maxMemoryMb",
        `Memory usage (${Math.round(heapUsedMb)} MB) exceeds limit (${this.config.maxMemoryMb} MB)`,
      );
    }
  }

  /**
   * Enforce session lifetime quota. Kills sessions that have exceeded
   * the configured maximum lifetime.
   */
  enforceLifetime(): void {
    if (this.disposed) return;

    const sessions = this.getSessions();
    const now = this.now();
    const maxLifetimeMs = this.config.maxSessionLifetimeHours * 60 * 60 * 1000;

    // A non-positive lifetime disables age-based killing. Long-running PTY
    // sessions should persist by default; admins can opt into cleanup.
    if (maxLifetimeMs <= 0) return;

    for (const session of sessions) {
      if (!session.isAlive) continue;

      const age = now - session.createdAt;
      if (age > maxLifetimeMs) {
        this.onWarning("Session exceeded lifetime quota, killing", {
          sessionId: session.sessionId,
          ageHours: Math.round(age / (60 * 60 * 1000) * 10) / 10,
          maxSessionLifetimeHours: this.config.maxSessionLifetimeHours,
        });
        session.kill();
      }
    }
  }

  // -------------------------------------------------------------------------
  // Private
  // -------------------------------------------------------------------------

  private loadConfig(): QuotaConfig {
    const defaults: QuotaConfig = {
      maxConcurrentSessions: DEFAULT_MAX_CONCURRENT_SESSIONS,
      maxMemoryMb: DEFAULT_MAX_MEMORY_MB,
      maxSessionLifetimeHours: DEFAULT_MAX_SESSION_LIFETIME_HOURS,
    };

    if (!this.fileExists(this.configPath)) {
      return defaults;
    }

    try {
      const raw = this.readFile(this.configPath);
      const parsed = JSON.parse(raw) as Partial<QuotaConfig>;

      return {
        maxConcurrentSessions: validPositiveInt(parsed.maxConcurrentSessions)
          ?? defaults.maxConcurrentSessions,
        maxMemoryMb: validPositiveNumber(parsed.maxMemoryMb)
          ?? defaults.maxMemoryMb,
        maxSessionLifetimeHours: validPositiveNumber(parsed.maxSessionLifetimeHours)
          ?? validZero(parsed.maxSessionLifetimeHours)
          ?? defaults.maxSessionLifetimeHours,
      };
    } catch (err) {
      this.onWarning("Failed to parse quotas config, using defaults", {
        path: this.configPath,
        error: (err as Error).message,
      });
      return defaults;
    }
  }
}

// ---------------------------------------------------------------------------
// Validation helpers
// ---------------------------------------------------------------------------

function validPositiveInt(value: unknown): number | null {
  if (typeof value !== "number" || !Number.isFinite(value) || value <= 0) {
    return null;
  }
  return Math.floor(value);
}

function validPositiveNumber(value: unknown): number | null {
  if (typeof value !== "number" || !Number.isFinite(value) || value <= 0) {
    return null;
  }
  return value;
}

function validZero(value: unknown): number | null {
  return value === 0 ? 0 : null;
}

// ---------------------------------------------------------------------------
// Default I/O implementations (overridable for testing)
// ---------------------------------------------------------------------------

function defaultReadFile(filePath: string): string {
  // Dynamic import to avoid issues in test environments
  const fs = require("node:fs") as typeof import("node:fs");
  return fs.readFileSync(filePath, "utf-8");
}

function defaultFileExists(filePath: string): boolean {
  const fs = require("node:fs") as typeof import("node:fs");
  return fs.existsSync(filePath);
}
