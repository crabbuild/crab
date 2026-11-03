/**
 * Structured JSON logging to stderr for the PTY daemon.
 *
 * Log level is configurable via CRAB_DAEMON_LOG_LEVEL env var.
 * Default: "info". Values: "error", "warn", "info", "debug".
 *
 * Each log line is a single JSON object written to stderr:
 *   { "ts": <ISO8601>, "level": <level>, "msg": <message>, ...fields }
 */

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

export type LogLevel = "error" | "warn" | "info" | "debug";

interface LogEntry {
  ts: string;
  level: LogLevel;
  msg: string;
  [key: string]: unknown;
}

// ---------------------------------------------------------------------------
// Level ordering (lower = more severe)
// ---------------------------------------------------------------------------

const LEVEL_ORDER: Record<LogLevel, number> = {
  error: 0,
  warn: 1,
  info: 2,
  debug: 3,
};

// ---------------------------------------------------------------------------
// Logger
// ---------------------------------------------------------------------------

export class DaemonLogger {
  private level: LogLevel;

  constructor(level?: LogLevel) {
    this.level = level ?? parseLogLevel(process.env.CRAB_DAEMON_LOG_LEVEL);
  }

  error(msg: string, fields?: Record<string, unknown>): void {
    this.log("error", msg, fields);
  }

  warn(msg: string, fields?: Record<string, unknown>): void {
    this.log("warn", msg, fields);
  }

  info(msg: string, fields?: Record<string, unknown>): void {
    this.log("info", msg, fields);
  }

  debug(msg: string, fields?: Record<string, unknown>): void {
    this.log("debug", msg, fields);
  }

  /**
   * Create a child logger with preset fields merged into every entry.
   */
  child(baseFields: Record<string, unknown>): ChildLogger {
    return new ChildLogger(this, baseFields);
  }

  /**
   * Write a log entry if the message level is at or below the configured threshold.
   */
  log(level: LogLevel, msg: string, fields?: Record<string, unknown>): void {
    if (LEVEL_ORDER[level] > LEVEL_ORDER[this.level]) return;

    const entry: LogEntry = {
      ts: new Date().toISOString(),
      level,
      msg,
      ...fields,
    };

    // Write as a single line to stderr — atomic for lines < PIPE_BUF (4096)
    process.stderr.write(JSON.stringify(entry) + "\n");
  }
}

// ---------------------------------------------------------------------------
// Child Logger
// ---------------------------------------------------------------------------

class ChildLogger {
  private parent: DaemonLogger;
  private baseFields: Record<string, unknown>;

  constructor(parent: DaemonLogger, baseFields: Record<string, unknown>) {
    this.parent = parent;
    this.baseFields = baseFields;
  }

  error(msg: string, fields?: Record<string, unknown>): void {
    this.parent.log("error", msg, { ...this.baseFields, ...fields });
  }

  warn(msg: string, fields?: Record<string, unknown>): void {
    this.parent.log("warn", msg, { ...this.baseFields, ...fields });
  }

  info(msg: string, fields?: Record<string, unknown>): void {
    this.parent.log("info", msg, { ...this.baseFields, ...fields });
  }

  debug(msg: string, fields?: Record<string, unknown>): void {
    this.parent.log("debug", msg, { ...this.baseFields, ...fields });
  }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

function parseLogLevel(value: string | undefined): LogLevel {
  if (!value) return "info";
  const normalized = value.toLowerCase().trim();
  if (normalized in LEVEL_ORDER) return normalized as LogLevel;
  return "info";
}
