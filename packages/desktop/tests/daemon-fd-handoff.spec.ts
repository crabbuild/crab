import { describe, it, expect, afterEach } from "vitest";
import * as fs from "fs";
import * as path from "path";
import * as os from "os";
import {
  serializeSessions,
  writeSnapshot,
  readSnapshot,
  adoptSnapshot,
  FD_OFFSET,
  SNAPSHOT_VERSION,
  type LiveSessionInfo,
  type HandoffSessionEntry,
  type HandoffSnapshot,
} from "../electron/daemon/fd-handoff";

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

function makeTmpDir(): string {
  return fs.mkdtempSync(path.join(os.tmpdir(), "fd-handoff-test-"));
}

function makeSession(overrides: Partial<LiveSessionInfo> = {}): LiveSessionInfo {
  return {
    id: overrides.id ?? "repo::/wt/test@@abc123",
    pid: overrides.pid ?? 12345,
    initialCwd: "initialCwd" in overrides ? overrides.initialCwd : undefined,
    cwd: "cwd" in overrides ? overrides.cwd! : "/home/user/project",
    cols: overrides.cols ?? 80,
    rows: overrides.rows ?? 24,
    agentMeta: overrides.agentMeta,
    pendingBuffer: overrides.pendingBuffer,
  };
}

// ---------------------------------------------------------------------------
// Cleanup
// ---------------------------------------------------------------------------

const tmpDirs: string[] = [];

afterEach(() => {
  for (const dir of tmpDirs) {
    fs.rmSync(dir, { recursive: true, force: true });
  }
  tmpDirs.length = 0;
});

// ---------------------------------------------------------------------------
// serializeSessions
// ---------------------------------------------------------------------------

describe("serializeSessions", () => {
  it("serializes an empty session list", () => {
    const result = serializeSessions([]);
    expect(result).toEqual([]);
  });

  it("assigns fdIndex starting at FD_OFFSET (4)", () => {
    const sessions = [makeSession({ id: "s1" }), makeSession({ id: "s2" })];
    const result = serializeSessions(sessions);

    expect(result[0].fdIndex).toBe(FD_OFFSET);
    expect(result[1].fdIndex).toBe(FD_OFFSET + 1);
  });

  it("captures id, pid, meta fields correctly", () => {
    const session = makeSession({
      id: "repo::/wt/feature@@deadbeef",
      pid: 99999,
      initialCwd: "/wt/feature",
      cwd: "/tmp/work",
      cols: 120,
      rows: 40,
      agentMeta: { taskId: "task-1", agentType: "claude", worktreePath: "/wt" },
    });

    const [entry] = serializeSessions([session]);

    expect(entry.id).toBe("repo::/wt/feature@@deadbeef");
    expect(entry.pid).toBe(99999);
    expect(entry.meta).toEqual({
      agentMeta: { taskId: "task-1", agentType: "claude", worktreePath: "/wt" },
      initialCwd: "/wt/feature",
      cwd: "/tmp/work",
      cols: 120,
      rows: 40,
    });
  });

  it("falls back to cwd as initialCwd for older session info", () => {
    const session = makeSession({ cwd: "/workspace/task" });

    const [entry] = serializeSessions([session]);

    expect(entry.meta.initialCwd).toBe("/workspace/task");
  });

  it("encodes pending buffer as base64", () => {
    const buf = Buffer.from("hello world");
    const session = makeSession({ pendingBuffer: buf });

    const [entry] = serializeSessions([session]);

    expect(entry.buffer).toBe(buf.toString("base64"));
  });

  it("uses empty string for buffer when no pending data", () => {
    const session = makeSession({ pendingBuffer: undefined });

    const [entry] = serializeSessions([session]);

    expect(entry.buffer).toBe("");
  });

  it("handles sessions with null cwd", () => {
    const session = makeSession({ cwd: null });

    const [entry] = serializeSessions([session]);

    expect(entry.meta.cwd).toBeNull();
  });

  it("preserves order and assigns sequential fdIndex values", () => {
    const sessions = Array.from({ length: 5 }, (_, i) =>
      makeSession({ id: `session-${i}`, pid: 1000 + i }),
    );

    const result = serializeSessions(sessions);

    for (let i = 0; i < 5; i++) {
      expect(result[i].id).toBe(`session-${i}`);
      expect(result[i].pid).toBe(1000 + i);
      expect(result[i].fdIndex).toBe(FD_OFFSET + i);
    }
  });
});

// ---------------------------------------------------------------------------
// writeSnapshot
// ---------------------------------------------------------------------------

describe("writeSnapshot", () => {
  it("writes a valid JSON file to the specified path", () => {
    const dir = makeTmpDir();
    tmpDirs.push(dir);
    const snapshotPath = path.join(dir, "snapshot.json");

    const entries = serializeSessions([makeSession()]);
    writeSnapshot(snapshotPath, entries);

    const raw = fs.readFileSync(snapshotPath, "utf-8");
    const parsed = JSON.parse(raw);

    expect(parsed.version).toBe(SNAPSHOT_VERSION);
    expect(parsed.sessions).toHaveLength(1);
    expect(parsed.createdAt).toBeGreaterThan(0);
  });

  it("creates parent directories if they do not exist", () => {
    const dir = makeTmpDir();
    tmpDirs.push(dir);
    const snapshotPath = path.join(dir, "nested", "deep", "snapshot.json");

    const entries = serializeSessions([makeSession()]);
    writeSnapshot(snapshotPath, entries);

    expect(fs.existsSync(snapshotPath)).toBe(true);
  });

  it("writes file with 0600 permissions", () => {
    const dir = makeTmpDir();
    tmpDirs.push(dir);
    const snapshotPath = path.join(dir, "snapshot.json");

    writeSnapshot(snapshotPath, []);

    const stat = fs.statSync(snapshotPath);
    // Check owner-only read/write (0600 = 0o600 = 384 decimal)
    const mode = stat.mode & 0o777;
    expect(mode).toBe(0o600);
  });

  it("overwrites an existing snapshot atomically", () => {
    const dir = makeTmpDir();
    tmpDirs.push(dir);
    const snapshotPath = path.join(dir, "snapshot.json");

    // Write first snapshot
    writeSnapshot(snapshotPath, serializeSessions([makeSession({ id: "first" })]));

    // Write second snapshot (should overwrite)
    writeSnapshot(snapshotPath, serializeSessions([makeSession({ id: "second" })]));

    const raw = fs.readFileSync(snapshotPath, "utf-8");
    const parsed = JSON.parse(raw);
    expect(parsed.sessions[0].id).toBe("second");
  });

  it("does not leave tmp files on success", () => {
    const dir = makeTmpDir();
    tmpDirs.push(dir);
    const snapshotPath = path.join(dir, "snapshot.json");

    writeSnapshot(snapshotPath, []);

    const files = fs.readdirSync(dir);
    expect(files).toEqual(["snapshot.json"]);
  });
});

// ---------------------------------------------------------------------------
// readSnapshot
// ---------------------------------------------------------------------------

describe("readSnapshot", () => {
  it("reads and parses a valid snapshot file", () => {
    const dir = makeTmpDir();
    tmpDirs.push(dir);
    const snapshotPath = path.join(dir, "snapshot.json");

    const entries = serializeSessions([makeSession({ id: "test-session" })]);
    writeSnapshot(snapshotPath, entries);

    const snapshot = readSnapshot(snapshotPath);

    expect(snapshot.version).toBe(SNAPSHOT_VERSION);
    expect(snapshot.sessions).toHaveLength(1);
    expect(snapshot.sessions[0].id).toBe("test-session");
  });

  it("throws on unsupported version", () => {
    const dir = makeTmpDir();
    tmpDirs.push(dir);
    const snapshotPath = path.join(dir, "snapshot.json");

    const badSnapshot = { version: 999, createdAt: Date.now(), sessions: [] };
    fs.writeFileSync(snapshotPath, JSON.stringify(badSnapshot));

    expect(() => readSnapshot(snapshotPath)).toThrow(
      "Unsupported handoff snapshot version: 999",
    );
  });

  it("throws on missing file", () => {
    expect(() => readSnapshot("/nonexistent/path/snapshot.json")).toThrow();
  });
});

// ---------------------------------------------------------------------------
// adoptSnapshot
// ---------------------------------------------------------------------------

describe("adoptSnapshot", () => {
  it("rebuilds session state from a valid snapshot", () => {
    const buf = Buffer.from("pending output data");
    const sessions: LiveSessionInfo[] = [
      makeSession({
        id: "session-a",
        pid: 5000,
        initialCwd: "/workspace/agent-task",
        cwd: "/home/dev",
        cols: 100,
        rows: 30,
        agentMeta: { taskId: "t1", agentType: "claude", worktreePath: "/wt" },
        pendingBuffer: buf,
      }),
    ];

    const entries = serializeSessions(sessions);
    const snapshot: HandoffSnapshot = {
      version: SNAPSHOT_VERSION,
      createdAt: Date.now(),
      sessions: entries,
    };

    const adopted = adoptSnapshot(snapshot);

    expect(adopted).toHaveLength(1);
    expect(adopted[0].id).toBe("session-a");
    expect(adopted[0].pid).toBe(5000);
    expect(adopted[0].fd).toBe(FD_OFFSET);
    expect(adopted[0].meta.initialCwd).toBe("/workspace/agent-task");
    expect(adopted[0].meta.cwd).toBe("/home/dev");
    expect(adopted[0].meta.cols).toBe(100);
    expect(adopted[0].meta.rows).toBe(30);
    expect(adopted[0].meta.agentMeta).toEqual({
      taskId: "t1",
      agentType: "claude",
      worktreePath: "/wt",
    });
    expect(adopted[0].buffer.toString()).toBe("pending output data");
  });

  it("decodes base64 buffer back to original bytes", () => {
    // Use binary data that isn't valid UTF-8
    const binaryData = Buffer.from([0x00, 0xff, 0x80, 0x7f, 0x01]);
    const sessions = [makeSession({ pendingBuffer: binaryData })];
    const entries = serializeSessions(sessions);

    const snapshot: HandoffSnapshot = {
      version: SNAPSHOT_VERSION,
      createdAt: Date.now(),
      sessions: entries,
    };

    const adopted = adoptSnapshot(snapshot);
    expect(Buffer.compare(adopted[0].buffer, binaryData)).toBe(0);
  });

  it("returns empty buffer when entry has no pending data", () => {
    const sessions = [makeSession({ pendingBuffer: undefined })];
    const entries = serializeSessions(sessions);

    const snapshot: HandoffSnapshot = {
      version: SNAPSHOT_VERSION,
      createdAt: Date.now(),
      sessions: entries,
    };

    const adopted = adoptSnapshot(snapshot);
    expect(adopted[0].buffer.length).toBe(0);
  });

  it("handles multiple sessions with correct fd mapping", () => {
    const sessions = Array.from({ length: 3 }, (_, i) =>
      makeSession({ id: `s${i}`, pid: 2000 + i }),
    );
    const entries = serializeSessions(sessions);

    const snapshot: HandoffSnapshot = {
      version: SNAPSHOT_VERSION,
      createdAt: Date.now(),
      sessions: entries,
    };

    const adopted = adoptSnapshot(snapshot);

    expect(adopted).toHaveLength(3);
    expect(adopted[0].fd).toBe(FD_OFFSET);
    expect(adopted[1].fd).toBe(FD_OFFSET + 1);
    expect(adopted[2].fd).toBe(FD_OFFSET + 2);
  });

  it("throws on unsupported snapshot version", () => {
    const snapshot: HandoffSnapshot = {
      version: 42,
      createdAt: Date.now(),
      sessions: [],
    };

    expect(() => adoptSnapshot(snapshot)).toThrow(
      "Unsupported handoff snapshot version: 42",
    );
  });

  it("throws on invalid fdIndex below FD_OFFSET", () => {
    const snapshot: HandoffSnapshot = {
      version: SNAPSHOT_VERSION,
      createdAt: Date.now(),
      sessions: [
        {
          id: "bad-session",
          pid: 1234,
          meta: { cwd: "/tmp", cols: 80, rows: 24 },
          fdIndex: 2, // Invalid: below FD_OFFSET (4)
          buffer: "",
        },
      ],
    };

    expect(() => adoptSnapshot(snapshot)).toThrow(
      "Invalid fdIndex 2 for session bad-session: must be >= 4",
    );
  });

  it("handles empty session list", () => {
    const snapshot: HandoffSnapshot = {
      version: SNAPSHOT_VERSION,
      createdAt: Date.now(),
      sessions: [],
    };

    const adopted = adoptSnapshot(snapshot);
    expect(adopted).toEqual([]);
  });
});

// ---------------------------------------------------------------------------
// End-to-end: serialize → write → read → adopt
// ---------------------------------------------------------------------------

describe("serialize → write → adopt round-trip", () => {
  it("produces valid sessions through the full pipeline", () => {
    const dir = makeTmpDir();
    tmpDirs.push(dir);
    const snapshotPath = path.join(dir, "handoff-snapshot.json");

    // Create live sessions with various configurations
    const liveSessions: LiveSessionInfo[] = [
      makeSession({
        id: "repo::/wt/main@@sess001",
        pid: 10001,
        cwd: "/home/user/main",
        cols: 120,
        rows: 40,
        pendingBuffer: Buffer.from("$ cargo build\r\n"),
      }),
      makeSession({
        id: "repo::/wt/feature@@sess002",
        pid: 10002,
        cwd: "/home/user/feature",
        cols: 80,
        rows: 24,
        agentMeta: {
          taskId: "task-42",
          agentType: "claude",
          worktreePath: "/home/user/feature",
        },
        pendingBuffer: Buffer.from([0x1b, 0x5b, 0x48]), // ESC[H cursor home
      }),
      makeSession({
        id: "repo::/wt/hotfix@@sess003",
        pid: 10003,
        cwd: null,
        cols: 200,
        rows: 50,
        // No pending buffer
      }),
    ];

    // Step 1: Serialize
    const entries = serializeSessions(liveSessions);
    expect(entries).toHaveLength(3);

    // Step 2: Write to disk
    writeSnapshot(snapshotPath, entries);
    expect(fs.existsSync(snapshotPath)).toBe(true);

    // Step 3: Read from disk
    const snapshot = readSnapshot(snapshotPath);

    // Step 4: Adopt
    const adopted = adoptSnapshot(snapshot);

    // Verify all sessions came through correctly
    expect(adopted).toHaveLength(3);

    // Session 1: user session with pending output
    expect(adopted[0].id).toBe("repo::/wt/main@@sess001");
    expect(adopted[0].pid).toBe(10001);
    expect(adopted[0].fd).toBe(4);
    expect(adopted[0].meta.cwd).toBe("/home/user/main");
    expect(adopted[0].meta.cols).toBe(120);
    expect(adopted[0].meta.rows).toBe(40);
    expect(adopted[0].meta.agentMeta).toBeUndefined();
    expect(adopted[0].buffer.toString()).toBe("$ cargo build\r\n");

    // Session 2: agent session with binary pending data
    expect(adopted[1].id).toBe("repo::/wt/feature@@sess002");
    expect(adopted[1].pid).toBe(10002);
    expect(adopted[1].fd).toBe(5);
    expect(adopted[1].meta.agentMeta).toEqual({
      taskId: "task-42",
      agentType: "claude",
      worktreePath: "/home/user/feature",
    });
    expect(Buffer.compare(adopted[1].buffer, Buffer.from([0x1b, 0x5b, 0x48]))).toBe(0);

    // Session 3: session with null cwd and no buffer
    expect(adopted[2].id).toBe("repo::/wt/hotfix@@sess003");
    expect(adopted[2].pid).toBe(10003);
    expect(adopted[2].fd).toBe(6);
    expect(adopted[2].meta.cwd).toBeNull();
    expect(adopted[2].meta.cols).toBe(200);
    expect(adopted[2].meta.rows).toBe(50);
    expect(adopted[2].buffer.length).toBe(0);
  });
});

// ---------------------------------------------------------------------------
// Successor-side handoff tests
//
// These tests spawn a real daemon-main.cjs child process with --handoff and
// --snapshot flags, using the fork mock pattern from prepare-upgrade.spec.ts
// for the IPC channel setup. They verify:
//   1. The successor sends upgrade-ack after successfully adopting sessions
//   2. The successor sends upgrade-nak when the snapshot is corrupt
//   3. Pending buffer data is replayed into the headless emulator so a
//      connecting client receives it in the createOrAttach snapshot
// ---------------------------------------------------------------------------

import * as child_process from "node:child_process";
import * as net from "node:net";
import {
  PROTOCOL_VERSION,
  encodeFrame,
  FrameDecoder,
  type HelloMessage,
  type HelloResponse,
  type DaemonRequest,
  type DaemonResponse,
  type CreateOrAttachResult,
} from "../electron/daemon/protocol";

// Path to the compiled daemon entry point
const DAEMON_MAIN_CJS = path.join(
  __dirname,
  "..",
  "dist-electron",
  "daemon",
  "daemon-main.cjs",
);

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/**
 * Create a connected socket pair and return the client socket's fd.
 * The server keeps the connection open so the read stream won't get EOF.
 * Returns the fd and a cleanup function.
 *
 * We use a Unix domain socket so the fd is a real socket fd that the
 * AdoptedPtySubprocessHandle can create streams on without getting immediate EOF.
 */
let socketPairCounter = 0;

async function createSocketFd(dir: string): Promise<{ fd: number; cleanup: () => void }> {
  const sockPath = path.join(dir, `test-pair-${++socketPairCounter}-${process.pid}.sock`);
  let serverConn: net.Socket | null = null;

  const server = net.createServer((conn) => {
    serverConn = conn;
    // Keep the connection open — don't close it
  });

  await new Promise<void>((resolve) => server.listen(sockPath, resolve));

  const client = net.createConnection(sockPath);
  await new Promise<void>((resolve, reject) => {
    client.on("connect", resolve);
    client.on("error", reject);
  });

  // Access the internal fd — this is the only way to get the fd from a Node.js socket
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  const fd: number = (client as any)._handle?.fd ?? -1;

  if (fd < 0) {
    client.destroy();
    serverConn?.destroy();
    server.close();
    throw new Error("Could not get socket fd from client socket");
  }

  const cleanup = () => {
    try { client.destroy(); } catch {}
    try { serverConn?.destroy(); } catch {}
    server.close();
    try { fs.unlinkSync(sockPath); } catch {}
  };

  return { fd, cleanup };
}

/**
 * Build a minimal handoff snapshot with one session.
 * The fdIndex must match the position in the successor's stdio array (4 = first PTY fd).
 */
function makeHandoffSnapshot(opts: {
  sessionId?: string;
  pid?: number;
  pendingBuffer?: Buffer;
  fdIndex?: number;
}): HandoffSnapshot {
  const {
    sessionId = "test-session@@abc123",
    pid = 99999,
    pendingBuffer,
    fdIndex = FD_OFFSET,
  } = opts;

  return {
    version: SNAPSHOT_VERSION,
    createdAt: Date.now(),
    sessions: [
      {
        id: sessionId,
        pid,
        meta: { cwd: "/tmp", cols: 80, rows: 24 },
        fdIndex,
        buffer: pendingBuffer ? pendingBuffer.toString("base64") : "",
      },
    ],
  };
}

/**
 * Spawn daemon-main.cjs in handoff mode with the given snapshot path and
 * PTY fds passed via stdio. Returns the child process.
 *
 * stdio layout: [ignore, inherit, inherit, ipc, ...ptyFds]
 */
function spawnSuccessor(opts: {
  socketPath: string;
  snapshotPath: string;
  ptyFds: number[];
}): child_process.ChildProcess {
  const stdio: Array<"ignore" | "inherit" | "ipc" | number> = [
    "ignore",
    "inherit",
    "inherit",
    "ipc",
    ...opts.ptyFds,
  ];

  return child_process.fork(
    DAEMON_MAIN_CJS,
    [
      "--socket", opts.socketPath,
      "--handoff",
      "--snapshot", opts.snapshotPath,
    ],
    {
      stdio: stdio as child_process.StdioOptions,
      detached: false,
      env: {
        ...process.env,
        ELECTRON_RUN_AS_NODE: "1",
      },
    },
  );
}

/**
 * Wait for the successor to send upgrade-ack or upgrade-nak via IPC.
 * Resolves with the message type and optional error.
 */
function waitForUpgradeMessage(
  child: child_process.ChildProcess,
  timeoutMs = 10_000,
): Promise<{ type: "upgrade-ack" | "upgrade-nak"; error?: string }> {
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => {
      reject(new Error("Timed out waiting for upgrade-ack/nak"));
    }, timeoutMs);

    const onMessage = (msg: unknown) => {
      const m = msg as { type: string; error?: string };
      if (m?.type === "upgrade-ack" || m?.type === "upgrade-nak") {
        clearTimeout(timer);
        child.removeListener("message", onMessage);
        resolve(m as { type: "upgrade-ack" | "upgrade-nak"; error?: string });
      }
    };

    child.on("message", onMessage);
  });
}

/**
 * Wait for the socket file to appear, retrying until the deadline.
 */
async function waitForSocket(socketPath: string, timeoutMs = 8_000): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (fs.existsSync(socketPath)) return;
    await new Promise((r) => setTimeout(r, 50));
  }
  throw new Error(`Socket file not available after ${timeoutMs}ms: ${socketPath}`);
}

/**
 * Connect a raw control + stream socket pair to the daemon at socketPath,
 * perform the hello handshake, and return both sockets.
 * Retries the connection if the socket is not yet ready.
 */
async function connectRawClient(
  socketPath: string,
  token: string,
): Promise<{ control: net.Socket; stream: net.Socket; clientId: string }> {
  const clientId = `test-client-${Date.now()}`;

  // Wait for the socket file to exist before attempting connection
  await waitForSocket(socketPath);

  const connectSocket = (role: "control" | "stream"): Promise<net.Socket> => {
    const deadline = Date.now() + 8_000;

    const attempt = (): Promise<net.Socket> =>
      new Promise((resolve, reject) => {
        const sock = net.createConnection(socketPath);
        const decoder = new FrameDecoder();

        const timer = setTimeout(() => {
          sock.destroy();
          reject(new Error(`Connect timeout for ${role} socket`));
        }, 5_000);

        sock.on("connect", () => {
          const hello: HelloMessage = {
            type: "hello",
            version: PROTOCOL_VERSION,
            token,
            clientId,
            role,
          };
          sock.write(encodeFrame(hello));
        });

        sock.on("data", (chunk: Buffer) => {
          decoder.push(chunk);
          const frames = decoder.frames();
          for (const frame of frames) {
            const resp = frame.json as HelloResponse;
            clearTimeout(timer);
            if (resp.ok) {
              resolve(sock);
            } else {
              sock.destroy();
              reject(new Error(`Hello rejected: ${resp.error ?? "unknown"}`));
            }
            return;
          }
        });

        sock.on("error", async (err) => {
          clearTimeout(timer);
          // Retry on ENOENT/ECONNREFUSED if we still have time
          if (
            Date.now() < deadline &&
            (err.message.includes("ENOENT") || err.message.includes("ECONNREFUSED"))
          ) {
            await new Promise((r) => setTimeout(r, 50));
            attempt().then(resolve, reject);
          } else {
            reject(err);
          }
        });
      });

    return attempt();
  };

  const control = await connectSocket("control");
  const stream = await connectSocket("stream");
  return { control, stream, clientId };
}

/**
 * Send a createOrAttach request over the control socket and wait for the response.
 */
function sendCreateOrAttach(
  control: net.Socket,
  sessionId: string,
): Promise<CreateOrAttachResult> {
  return new Promise((resolve, reject) => {
    const reqId = `req-${Date.now()}`;
    const decoder = new FrameDecoder();

    const timer = setTimeout(() => {
      reject(new Error("createOrAttach timed out"));
    }, 5_000);

    const onData = (chunk: Buffer) => {
      decoder.push(chunk);
      const frames = decoder.frames();
      for (const frame of frames) {
        const resp = frame.json as DaemonResponse;
        if (resp.id === reqId) {
          clearTimeout(timer);
          control.removeListener("data", onData);
          if (resp.ok) {
            resolve(resp.payload as CreateOrAttachResult);
          } else {
            reject(new Error(resp.error));
          }
          return;
        }
      }
    };

    control.on("data", onData);

    const req: DaemonRequest = {
      id: reqId,
      type: "createOrAttach",
      payload: {
        sessionId,
        cols: 80,
        rows: 24,
        cwd: "/tmp",
      },
    };
    control.write(encodeFrame(req));
  });
}

/**
 * Read the auth token from the daemon's token file, retrying until it appears.
 */
async function readTokenWithRetry(
  tokenPath: string,
  timeoutMs = 8_000,
): Promise<string> {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    try {
      const token = fs.readFileSync(tokenPath, "utf-8").trim();
      if (token) return token;
    } catch {
      // Not written yet
    }
    await new Promise((r) => setTimeout(r, 50));
  }
  throw new Error(`Token file not available after ${timeoutMs}ms: ${tokenPath}`);
}

// ---------------------------------------------------------------------------
// Cleanup tracking
// ---------------------------------------------------------------------------

const successorProcesses: child_process.ChildProcess[] = [];
const socketCleanups: Array<() => void> = [];

afterEach(() => {
  // Kill any lingering successor processes
  for (const child of successorProcesses) {
    try {
      if (!child.killed) child.kill("SIGKILL");
    } catch {
      // Already exited
    }
  }
  successorProcesses.length = 0;

  // Clean up socket pairs
  for (const cleanup of socketCleanups) {
    try { cleanup(); } catch {}
  }
  socketCleanups.length = 0;
});

// ---------------------------------------------------------------------------
// Successor-side: upgrade-ack
// ---------------------------------------------------------------------------

describe("successor handoff: upgrade-ack", () => {
  it("sends upgrade-ack after successfully adopting a session from snapshot", async () => {
    const dir = makeTmpDir();
    tmpDirs.push(dir);

    const socketPath = path.join(dir, "daemon-v2.sock");
    const snapshotPath = path.join(dir, "handoff-snapshot.json");

    // Use a connected socket pair so the AdoptedPtySubprocessHandle read stream
    // won't get immediate EOF (which would crash the successor with EBADF)
    const { fd: sockFd, cleanup } = await createSocketFd(dir);
    socketCleanups.push(cleanup);

    // Write snapshot with one session whose fdIndex matches stdio position 4
    const snapshot = makeHandoffSnapshot({ fdIndex: FD_OFFSET });
    fs.writeFileSync(snapshotPath, JSON.stringify(snapshot));

    const child = spawnSuccessor({
      socketPath,
      snapshotPath,
      ptyFds: [sockFd],
    });
    successorProcesses.push(child);

    const msg = await waitForUpgradeMessage(child);

    expect(msg.type).toBe("upgrade-ack");
  }, 15_000);

  it("sends upgrade-ack for zero-session snapshot (no-op upgrade)", async () => {
    const dir = makeTmpDir();
    tmpDirs.push(dir);

    const socketPath = path.join(dir, "daemon-v2.sock");
    const snapshotPath = path.join(dir, "handoff-snapshot.json");

    const emptySnapshot: HandoffSnapshot = {
      version: SNAPSHOT_VERSION,
      createdAt: Date.now(),
      sessions: [],
    };
    fs.writeFileSync(snapshotPath, JSON.stringify(emptySnapshot));

    const child = spawnSuccessor({
      socketPath,
      snapshotPath,
      ptyFds: [],
    });
    successorProcesses.push(child);

    const msg = await waitForUpgradeMessage(child);

    expect(msg.type).toBe("upgrade-ack");
  }, 15_000);

  it("sends upgrade-ack for multiple sessions", async () => {
    const dir = makeTmpDir();
    tmpDirs.push(dir);

    const socketPath = path.join(dir, "daemon-v2.sock");
    const snapshotPath = path.join(dir, "handoff-snapshot.json");

    // Create two socket pairs for two sessions
    const { fd: fd1, cleanup: cleanup1 } = await createSocketFd(dir);
    const { fd: fd2, cleanup: cleanup2 } = await createSocketFd(dir);
    socketCleanups.push(cleanup1, cleanup2);

    const multiSnapshot: HandoffSnapshot = {
      version: SNAPSHOT_VERSION,
      createdAt: Date.now(),
      sessions: [
        {
          id: "session-one@@abc",
          pid: 11111,
          meta: { cwd: "/home/user", cols: 80, rows: 24 },
          fdIndex: FD_OFFSET,
          buffer: "",
        },
        {
          id: "session-two@@def",
          pid: 22222,
          meta: { cwd: "/home/user/project", cols: 120, rows: 40 },
          fdIndex: FD_OFFSET + 1,
          buffer: "",
        },
      ],
    };
    fs.writeFileSync(snapshotPath, JSON.stringify(multiSnapshot));

    const child = spawnSuccessor({
      socketPath,
      snapshotPath,
      ptyFds: [fd1, fd2],
    });
    successorProcesses.push(child);

    const msg = await waitForUpgradeMessage(child);

    expect(msg.type).toBe("upgrade-ack");
  }, 15_000);
});

// ---------------------------------------------------------------------------
// Successor-side: upgrade-nak
// ---------------------------------------------------------------------------

describe("successor handoff: upgrade-nak", () => {
  it("sends upgrade-nak when snapshot file is corrupt JSON", async () => {
    const dir = makeTmpDir();
    tmpDirs.push(dir);

    const socketPath = path.join(dir, "daemon-v2.sock");
    const snapshotPath = path.join(dir, "handoff-snapshot.json");

    // Write invalid JSON
    fs.writeFileSync(snapshotPath, "{ this is not valid json }");

    const child = spawnSuccessor({
      socketPath,
      snapshotPath,
      ptyFds: [],
    });
    successorProcesses.push(child);

    const msg = await waitForUpgradeMessage(child);

    expect(msg.type).toBe("upgrade-nak");
    expect(msg.error).toBeTruthy();
  }, 15_000);

  it("sends upgrade-nak when snapshot has unsupported version", async () => {
    const dir = makeTmpDir();
    tmpDirs.push(dir);

    const socketPath = path.join(dir, "daemon-v2.sock");
    const snapshotPath = path.join(dir, "handoff-snapshot.json");

    const badSnapshot = {
      version: 999,
      createdAt: Date.now(),
      sessions: [],
    };
    fs.writeFileSync(snapshotPath, JSON.stringify(badSnapshot));

    const child = spawnSuccessor({
      socketPath,
      snapshotPath,
      ptyFds: [],
    });
    successorProcesses.push(child);

    const msg = await waitForUpgradeMessage(child);

    expect(msg.type).toBe("upgrade-nak");
    expect(msg.error).toContain("999");
  }, 15_000);

  it("sends upgrade-nak when snapshot file does not exist", async () => {
    const dir = makeTmpDir();
    tmpDirs.push(dir);

    const socketPath = path.join(dir, "daemon-v2.sock");
    const missingSnapshotPath = path.join(dir, "nonexistent-snapshot.json");

    const child = spawnSuccessor({
      socketPath,
      snapshotPath: missingSnapshotPath,
      ptyFds: [],
    });
    successorProcesses.push(child);

    const msg = await waitForUpgradeMessage(child);

    expect(msg.type).toBe("upgrade-nak");
    expect(msg.error).toBeTruthy();
  }, 15_000);
});

// ---------------------------------------------------------------------------
// Successor-side: pending buffer replay
// ---------------------------------------------------------------------------

describe("successor handoff: pending buffer replay", () => {
  it("replays pending buffer into headless emulator so createOrAttach snapshot contains it", async () => {
    const dir = makeTmpDir();
    tmpDirs.push(dir);

    const socketPath = path.join(dir, "daemon-v2.sock");
    const tokenPath = path.join(dir, "daemon-v2.token");
    const snapshotPath = path.join(dir, "handoff-snapshot.json");

    const sessionId = "replay-test@@sess001";
    // Use a simple ASCII string that the headless emulator will render
    const pendingContent = "hello from predecessor\r\n";
    const pendingBuffer = Buffer.from(pendingContent);

    const { fd: sockFd, cleanup } = await createSocketFd(dir);
    socketCleanups.push(cleanup);

    const snapshot = makeHandoffSnapshot({
      sessionId,
      pid: 55555,
      pendingBuffer,
      fdIndex: FD_OFFSET,
    });
    fs.writeFileSync(snapshotPath, JSON.stringify(snapshot));

    const child = spawnSuccessor({
      socketPath,
      snapshotPath,
      ptyFds: [sockFd],
    });
    successorProcesses.push(child);

    // Wait for the successor to adopt sessions and send ack
    const msg = await waitForUpgradeMessage(child);
    expect(msg.type).toBe("upgrade-ack");

    // Read the auth token written by the successor
    const token = await readTokenWithRetry(tokenPath);

    // Connect a raw client to the successor
    const { control, stream } = await connectRawClient(socketPath, token);

    try {
      // Call createOrAttach — the session was adopted with the pending buffer
      // already replayed into the headless emulator
      const result = await sendCreateOrAttach(control, sessionId);

      // The session was adopted (not newly spawned), so isNew should be false
      expect(result.isNew).toBe(false);

      // The snapshot should contain the replayed buffer content
      expect(result.snapshot).not.toBeNull();
      expect(result.snapshot!.snapshotAnsi).toContain("hello from predecessor");
    } finally {
      control.destroy();
      stream.destroy();
    }
  }, 20_000);

  it("replays pending buffer for agent session and preserves agentMeta", async () => {
    const dir = makeTmpDir();
    tmpDirs.push(dir);

    const socketPath = path.join(dir, "daemon-v2.sock");
    const tokenPath = path.join(dir, "daemon-v2.token");
    const snapshotPath = path.join(dir, "handoff-snapshot.json");

    const sessionId = "agent-replay@@sess002";
    const pendingContent = "agent output line\r\n";

    const { fd: sockFd, cleanup } = await createSocketFd(dir);
    socketCleanups.push(cleanup);

    const agentSnapshot: HandoffSnapshot = {
      version: SNAPSHOT_VERSION,
      createdAt: Date.now(),
      sessions: [
        {
          id: sessionId,
          pid: 66666,
          meta: {
            cwd: "/workspace/agent",
            cols: 120,
            rows: 40,
            agentMeta: { taskId: "task-99", agentType: "claude", worktreePath: "/workspace/agent" },
          },
          fdIndex: FD_OFFSET,
          buffer: Buffer.from(pendingContent).toString("base64"),
        },
      ],
    };
    fs.writeFileSync(snapshotPath, JSON.stringify(agentSnapshot));

    const child = spawnSuccessor({
      socketPath,
      snapshotPath,
      ptyFds: [sockFd],
    });
    successorProcesses.push(child);

    const msg = await waitForUpgradeMessage(child);
    expect(msg.type).toBe("upgrade-ack");

    const token = await readTokenWithRetry(tokenPath);
    const { control, stream } = await connectRawClient(socketPath, token);

    try {
      const result = await sendCreateOrAttach(control, sessionId);

      expect(result.isNew).toBe(false);
      expect(result.snapshot).not.toBeNull();
      expect(result.snapshot!.snapshotAnsi).toContain("agent output line");
    } finally {
      control.destroy();
      stream.destroy();
    }
  }, 20_000);

  it("handles empty pending buffer — snapshot is empty but session is adopted", async () => {
    const dir = makeTmpDir();
    tmpDirs.push(dir);

    const socketPath = path.join(dir, "daemon-v2.sock");
    const tokenPath = path.join(dir, "daemon-v2.token");
    const snapshotPath = path.join(dir, "handoff-snapshot.json");

    const sessionId = "empty-buffer@@sess003";

    const { fd: sockFd, cleanup } = await createSocketFd(dir);
    socketCleanups.push(cleanup);

    const snapshot = makeHandoffSnapshot({
      sessionId,
      pid: 77777,
      pendingBuffer: undefined,
      fdIndex: FD_OFFSET,
    });
    fs.writeFileSync(snapshotPath, JSON.stringify(snapshot));

    const child = spawnSuccessor({
      socketPath,
      snapshotPath,
      ptyFds: [sockFd],
    });
    successorProcesses.push(child);

    const msg = await waitForUpgradeMessage(child);
    expect(msg.type).toBe("upgrade-ack");

    const token = await readTokenWithRetry(tokenPath);
    const { control, stream } = await connectRawClient(socketPath, token);

    try {
      const result = await sendCreateOrAttach(control, sessionId);

      // Session was adopted (not new)
      expect(result.isNew).toBe(false);
      // Snapshot exists but is empty (no pending data was replayed)
      expect(result.snapshot).not.toBeNull();
    } finally {
      control.destroy();
      stream.destroy();
    }
  }, 20_000);
});
