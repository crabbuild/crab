/**
 * PTY Daemon entry point — spawned by Electron main process as a
 * detached child with ELECTRON_RUN_AS_NODE=1.
 *
 * Responsibilities:
 * - Parse CLI args (--socket, --handoff, --snapshot)
 * - Detect and remove stale sockets via PID file verification
 * - Create DaemonServer (socket, auth, session management)
 * - Write PID file, send `ready` IPC message, disconnect IPC channel
 * - Handle SIGTERM/SIGINT for graceful shutdown
 * - Set process.title for identification in ps/top
 * - Structured JSON logging to stderr
 * - On Linux, reduce OOM kill likelihood via oom_score_adj
 * - Uncaught exception handler for graceful shutdown on crash
 */

import * as fs from "node:fs";
import * as path from "node:path";
import { socketAddress } from "./socket-address";
import { daemonControlRoot, prepareDaemonHistory } from "./daemon-sessions";

import { DaemonServer } from "./daemon-server";
import { createNodePtySpawner } from "./subprocess-handle";
import { DaemonLogger } from "./daemon-logger";
import { PROTOCOL_VERSION } from "./protocol";
import { ResourceQuotaManager } from "./resource-quotas";
import { HistoryManager, getSanitizedKeys } from "./history-manager";
import { CheckpointTimer } from "./checkpoint-timer";
import { SessionActivityBitmap } from "./session-activity-bitmap";
import { MemoryManager } from "./memory-management";
import { AuditLogger } from "./audit-logger";
import {
  readSnapshot,
  adoptSnapshot,
} from "./fd-handoff";
import {
  sendUpgradeAck,
  sendUpgradeNak,
  bindSocketWithRetry,
} from "./prepare-upgrade";

// ---------------------------------------------------------------------------
// Process identity
// ---------------------------------------------------------------------------

process.title = "crab-pty-daemon";

// ---------------------------------------------------------------------------
// Logger
// ---------------------------------------------------------------------------

const logger = new DaemonLogger();

// ---------------------------------------------------------------------------
// CLI argument parsing
// ---------------------------------------------------------------------------

interface DaemonArgs {
  socket: string;
  handoff: boolean;
  snapshot: string | null;
}

function parseArgs(argv: string[]): DaemonArgs {
  let socket = "";
  let handoff = false;
  let snapshot: string | null = null;

  for (let i = 2; i < argv.length; i++) {
    const arg = argv[i];
    if (arg === "--socket" && i + 1 < argv.length) {
      socket = argv[++i]!;
    } else if (arg === "--handoff") {
      handoff = true;
    } else if (arg === "--snapshot" && i + 1 < argv.length) {
      snapshot = argv[++i]!;
    } else if (arg?.startsWith("--socket=")) {
      socket = arg.slice("--socket=".length);
    } else if (arg?.startsWith("--snapshot=")) {
      snapshot = arg.slice("--snapshot=".length);
    }
  }

  if (!socket) {
    logger.error("Missing required --socket argument");
    process.exit(1);
  }

  return { socket, handoff, snapshot };
}

// ---------------------------------------------------------------------------
// Stale socket detection
// ---------------------------------------------------------------------------

interface PidFileData {
  pid: number;
  startedAtMs: number;
  entryPath: string;
}

function isProcessAlive(pid: number): boolean {
  try {
    // signal 0 checks existence without sending a signal
    process.kill(pid, 0);
    return true;
  } catch {
    return false;
  }
}

function cleanupStaleSocket(socketPath: string, pidPath: string): void {
  try {
    const pidData = JSON.parse(fs.readFileSync(pidPath, "utf-8")) as PidFileData;

    if (isProcessAlive(pidData.pid)) {
      // PID is alive — check if it's actually our daemon by verifying
      // the start time matches. If we can't verify, assume it's stale
      // since the new daemon is being asked to start.
      logger.warn("Existing daemon PID is alive, attempting socket probe", {
        pid: pidData.pid,
        startedAtMs: pidData.startedAtMs,
      });

      // Try connecting to the socket — if it responds, another daemon is running
      const probeResult = probeSocket(socketPath);
      if (probeResult) {
        logger.error("Another daemon is already running on this socket", {
          pid: pidData.pid,
        });
        process.exit(1);
      }

      // Socket doesn't respond — process is alive but not our daemon (PID reuse)
      logger.info("Stale PID file detected (PID reused by another process), cleaning up", {
        pid: pidData.pid,
      });
    } else {
      logger.info("Stale socket detected (process not alive), cleaning up", {
        pid: pidData.pid,
      });
    }
  } catch {
    // PID file doesn't exist or is malformed — just clean up the socket
    logger.debug("No valid PID file found, cleaning up stale socket");
  }

  // Remove stale socket file
  try {
    fs.unlinkSync(socketAddress(socketPath));
  } catch {
    // May not exist
  }

  // Remove stale PID file
  try {
    fs.unlinkSync(pidPath);
  } catch {
    // May not exist
  }

  // Remove stale token file
  const tokenPath = socketPath.replace(/\.sock$/, ".token");
  try {
    fs.unlinkSync(tokenPath);
  } catch {
    // May not exist
  }
}

/**
 * Synchronous socket probe — checks if a daemon is likely listening.
 *
 * For the daemon startup path, we rely on the PID liveness check
 * as the primary stale-detection signal. A full async probe with
 * hello handshake is done by DaemonSpawner (the client side).
 * Here we do a best-effort check: if the PID is alive and the
 * socket file exists, we conservatively assume the daemon is running.
 */
function probeSocket(_socketPath: string): boolean {
  // The caller (cleanupStaleSocket) only reaches this point when the
  // PID IS alive, so we return true to prevent clobbering a live daemon.
  return true;
}

// ---------------------------------------------------------------------------
// OOM score adjustment (Linux only)
// ---------------------------------------------------------------------------

function setOomScoreAdj(score: number): void {
  if (process.platform !== "linux") return;

  try {
    fs.writeFileSync(`/proc/${process.pid}/oom_score_adj`, String(score));
    logger.debug("Set oom_score_adj", { score });
  } catch (err) {
    // Non-fatal — may lack permissions
    logger.debug("Failed to set oom_score_adj (non-fatal)", {
      error: (err as Error).message,
    });
  }
}

// ---------------------------------------------------------------------------
// Graceful shutdown
// ---------------------------------------------------------------------------

let server: DaemonServer | null = null;
let historyManager: HistoryManager | null = null;
let checkpointTimer: CheckpointTimer | null = null;
let quotaManager: ResourceQuotaManager | null = null;
let memoryManager: MemoryManager | null = null;
let auditLogger: AuditLogger | null = null;
let isShuttingDown = false;

async function gracefulShutdown(reason: string, killSessions = true): Promise<void> {
  if (isShuttingDown) return;
  isShuttingDown = true;

  logger.info("Graceful shutdown initiated", { reason });

  if (!server) {
    process.exit(0);
    return;
  }

  try {
    // Persist latest state before closing sessions
    if (checkpointTimer) {
      checkpointTimer.stop();
      checkpointTimer.finalCheckpoint();
    }

    await server.shutdown({ killSessions });
    logger.info("Shutdown complete");
  } catch (err) {
    logger.error("Error during shutdown", { error: (err as Error).message });
  }

  // Dispose history manager after server is down
  if (historyManager) {
    historyManager.dispose();
  }

  // Dispose quota manager
  if (quotaManager) {
    quotaManager.dispose();
  }

  // Dispose memory manager
  if (memoryManager) {
    memoryManager.dispose();
  }

  // Dispose audit logger after all other subsystems
  if (auditLogger) {
    auditLogger.dispose();
  }

  process.exit(0);
}

// ---------------------------------------------------------------------------
// Signal handlers
// ---------------------------------------------------------------------------

process.on("SIGTERM", () => {
  void gracefulShutdown("SIGTERM");
});

process.on("SIGINT", () => {
  void gracefulShutdown("SIGINT");
});

// ---------------------------------------------------------------------------
// Uncaught exception / unhandled rejection handlers
// ---------------------------------------------------------------------------

process.on("uncaughtException", (err) => {
  logger.error("Uncaught exception — initiating graceful shutdown", {
    error: err.message,
    stack: err.stack,
  });
  void gracefulShutdown("uncaughtException");
});

process.on("unhandledRejection", (reason) => {
  const message = reason instanceof Error ? reason.message : String(reason);
  const stack = reason instanceof Error ? reason.stack : undefined;
  logger.error("Unhandled rejection — initiating graceful shutdown", {
    error: message,
    stack,
  });
  void gracefulShutdown("unhandledRejection");
});

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

async function main(): Promise<void> {
  const args = parseArgs(process.argv);

  logger.info("PTY daemon starting", {
    pid: process.pid,
    protocolVersion: PROTOCOL_VERSION,
    socketPath: args.socket,
    handoff: args.handoff,
    nodeVersion: process.version,
  });

  // Reduce OOM kill likelihood on Linux
  setOomScoreAdj(-100);

  // Derive paths from socket path
  const socketPath = args.socket;
  const socketDir = path.dirname(socketPath);
  const controlRoot = daemonControlRoot(socketDir);
  const baseName = path.basename(socketPath, ".sock");
  const tokenPath = path.join(socketDir, `${baseName}.token`);
  const pidPath = path.join(socketDir, `${baseName}.pid`);

  // Ensure socket directory exists
  fs.mkdirSync(socketDir, { recursive: true });

  // Write a quotas.json.example so users can discover the available keys.
  // This is best-effort — failure is non-fatal.
  try {
    const examplePath = path.join(controlRoot, "quotas.json.example");
    if (!fs.existsSync(examplePath)) {
      fs.writeFileSync(
        examplePath,
        JSON.stringify(
          {
            maxConcurrentSessions: 50,
            maxMemoryMb: 1024,
            maxSessionLifetimeHours: 0,
          },
          null,
          2,
        ) + "\n",
        "utf-8",
      );
    }
  } catch {
    // Non-fatal
  }

  // Check for stale socket and clean up if needed
  // Windows pipes have no socket file. The PID guard must still protect a live
  // daemon's token from being overwritten by a duplicate startup.
  if (fs.existsSync(socketAddress(socketPath)) || fs.existsSync(pidPath)) {
    cleanupStaleSocket(socketPath, pidPath);
  }

  // Publish ownership before claiming history. Concurrent app runtimes must
  // not move these checkpoints while this process starts its terminal host.
  fs.writeFileSync(pidPath, JSON.stringify({
    pid: process.pid, startedAtMs: Date.now(), entryPath: process.argv[1] ?? "",
  }), { mode: 0o600 });

  // Instantiate durability subsystems
  // Each owner checkpoints only its own sessions. Shared history would offer
  // cold restore for processes still running in a previous app runtime.
  historyManager = new HistoryManager({ basePath: prepareDaemonHistory(socketDir) });
  historyManager.prune();

  const activityTracker = new SessionActivityBitmap();

  // Instantiate AuditLogger — reads CRAB_DAEMON_AUDIT_LOG env var for the log path.
  // No-op when the env var is not set.
  auditLogger = new AuditLogger();

  // Instantiate ResourceQuotaManager — reads quotas.json from the socket dir.
  // Must be created before the server so the beforeSpawn hook is available.
  quotaManager = new ResourceQuotaManager(
    () => server?.terminalHost.getQuotaSessions() ?? [],
    {
      configPath: path.join(controlRoot, "quotas.json"),
      onWarning: (message, meta) => logger.warn(message, meta),
      onInfo: (message, meta) => logger.info(message, meta),
    },
  );
  quotaManager.start();

  // Create the daemon server with lifecycle hooks wired to durability subsystems
  server = new DaemonServer({
    socketPath,
    tokenPath,
    pidPath,
    spawnSubprocess: createNodePtySpawner(),
    onShutdownRequested: (reason, killSessions) => {
      void gracefulShutdown(reason, killSessions);
    },
    hooks: {
      beforeSpawn: () => quotaManager!.checkCanCreateSession(server!.sessionCount),
      onSessionCreated: (id, opts) => {
        const sanitizedKeys = getSanitizedKeys(opts.env);
        historyManager!.openSession(id, {
          cwd: opts.cwd ?? process.env.HOME ?? "/",
          cols: opts.cols,
          rows: opts.rows,
          env: opts.env,
          sanitizedKeys,
        });
        activityTracker.register(id);
        auditLogger!.sessionCreated(id, { agent: !!opts.agentMeta });
      },
      onSessionAttached: (id) => {
        auditLogger!.sessionAttached(id);
      },
      onSessionDetached: (id) => {
        auditLogger!.sessionDetached(id);
      },
      onSessionKilled: (id) => {
        auditLogger!.sessionKilled(id);
      },
      onSessionData: (id, _byteLength) => {
        activityTracker.markDirty(id);
      },
      onSessionExit: (id, code) => {
        historyManager!.closeSession(id, code);
        activityTracker.remove(id);
        checkpointTimer?.removeSession(id);
        auditLogger!.sessionExited(id, { code });
      },
      onAgentState: (id, state, prev) => {
        auditLogger!.agentStateChanged(id, state, prev);
      },
      onCheckpointRequested: (id) => {
        historyManager!.markDirty(id);
        const snapshot = server!.terminalHost.getSnapshot(id);
        if (snapshot) {
          historyManager!.checkpoint(id, snapshot);
        }
      },
      onColdRestoreResume: (id, opts) => {
        const sanitizedKeys = getSanitizedKeys(opts.env);
        historyManager!.openSession(id, {
          cwd: opts.cwd ?? process.env.HOME ?? "/",
          cols: opts.cols,
          rows: opts.rows,
          env: opts.env,
          sanitizedKeys,
        });
        activityTracker.register(id);
        auditLogger!.sessionCreated(id, { agent: false });
      },
      onColdRestoreDiscard: (id) => {
        historyManager!.removeSession(id);
      },
      getPersistedEnv: (id) => {
        return historyManager!.readMeta(id)?.env;
      },
    },
  });

  // Wire checkpoint timer now that server (and its terminalHost) exist
  checkpointTimer = new CheckpointTimer({
    historyManager,
    snapshotProvider: {
      getSnapshot: (id) => server!.terminalHost.getSnapshot(id),
    },
    activityTracker,
  });

  // Start the server (binds socket, writes token + PID files).
  // For the handoff successor path, use bindSocketWithRetry because the
  // predecessor may still hold the socket briefly after receiving the ack.
  // The entire handoff sequence is wrapped in a try/catch so any failure
  // sends upgrade-nak and exits non-zero.
  if (args.handoff && args.snapshot) {
    const snapshotPath = args.snapshot;
    try {
      const snapshot = readSnapshot(snapshotPath);
      const adopted = adoptSnapshot(snapshot);

      await bindSocketWithRetry(() => server!.start());

      server.adoptSessions(adopted);

      sendUpgradeAck();

      logger.info("Upgrade successor: sessions adopted, ack sent", {
        sessionCount: adopted.length,
      });

      // Best-effort: delete the snapshot file after successful adoption
      try {
        fs.unlinkSync(snapshotPath);
      } catch {
        // Non-fatal — file may have already been removed
      }
    } catch (err) {
      const reason = (err as Error).message;
      logger.error("Upgrade successor: failed to adopt sessions", { error: reason });
      sendUpgradeNak(reason);
      process.exit(1);
    }
  } else {
    await server.start();
  }

  // Scan for cold-restore sessions (unclean shutdown: meta.json without endedAt + checkpoint.json).
  // Broadcast a cold-restore-available event to all connected clients for each such session.
  // The renderer drives the actual resume — we do not auto-spawn anything here.
  const coldRestoreSessions = historyManager.listColdRestoreSessions();
  if (coldRestoreSessions.length > 0) {
    logger.info("Cold restore sessions detected", { count: coldRestoreSessions.length });

    // Defer the broadcast slightly so any clients that connect immediately after
    // server.start() have time to complete their hello handshake.
    setImmediate(() => {
      for (const session of coldRestoreSessions) {
        logger.info("Broadcasting cold-restore-available", { sessionId: session.sessionId });
        server!.broadcastToAllClients({
          type: "event",
          event: "cold-restore-available",
          sessionId: session.sessionId,
          payload: {
            snapshot: session.snapshot,
            savedAt: session.savedAt,
            sanitizedKeys: session.sanitizedKeys,
            cwd: session.cwd,
            cols: session.cols,
            rows: session.rows,
          },
        });
      }
    });
  }

  // Expose the audit log path to the diagnostics subsystem
  if (auditLogger.enabled) {
    server.setAuditLogPath(process.env.CRAB_DAEMON_AUDIT_LOG?.trim() ?? null);
  }

  // Start periodic checkpointing after server is ready
  checkpointTimer.start();

  // Instantiate MemoryManager — idle scrollback trimming, memory pressure
  // detection, and high-throughput mode for fast-scrolling sessions.
  let highThroughputSessionCount = 0;
  const memoryPressureBytes = 512 * 1024 * 1024; // 512 MB

  memoryManager = new MemoryManager(
    () => server!.terminalHost.listManagedSessions(),
    (sessionId, isHighThroughput) => {
      server!.terminalHost.setHighThroughput(sessionId, isHighThroughput);

      // Track count of sessions in high-throughput mode
      if (isHighThroughput) {
        highThroughputSessionCount++;
      } else {
        highThroughputSessionCount = Math.max(0, highThroughputSessionCount - 1);
      }
      server!.setHighThroughputSessionCount(highThroughputSessionCount);

      // Derive and update memory pressure level from current heap usage
      const heapUsed = process.memoryUsage().heapUsed;
      let pressure: "ok" | "warn" | "critical";
      if (heapUsed > memoryPressureBytes * 0.9) {
        pressure = "critical";
      } else if (heapUsed > memoryPressureBytes * 0.7) {
        pressure = "warn";
      } else {
        pressure = "ok";
      }
      server!.setMemoryPressure(pressure);
    },
  );
  memoryManager.start();

  logger.info("PTY daemon ready", {
    pid: process.pid,
    socketPath,
    protocolVersion: PROTOCOL_VERSION,
  });

  // Send `ready` IPC message to parent process, then disconnect
  if (process.send) {
    process.send({ type: "ready", pid: process.pid });

    // Disconnect the IPC channel so the parent can exit without waiting
    if (process.disconnect) {
      process.disconnect();
    }
  }
}

// ---------------------------------------------------------------------------
// Entry
// ---------------------------------------------------------------------------

main().catch((err) => {
  logger.error("Fatal error during daemon startup", {
    error: (err as Error).message,
    stack: (err as Error).stack,
  });
  process.exit(1);
});
