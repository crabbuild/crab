import { spawn, ChildProcess } from "node:child_process";
import * as net from "node:net";
import { ipcMain } from "electron";

// ── Types ────────────────────────────────────────────────────────────

export type SidecarId = "jupyter" | "tensorboard" | "mlflow-ui" | "duckdb";

export interface SidecarSpec {
  id: SidecarId;
  cwd: string;
  port?: number;
  env?: Record<string, string>;
}

export type SidecarStatus = "stopped" | "starting" | "running" | "error";

export interface SidecarState {
  id: SidecarId;
  status: SidecarStatus;
  port: number | null;
  url: string | null;
  error: string | null;
}

// ── Ring buffer ──────────────────────────────────────────────────────

const RING_BUFFER_CAPACITY = 200;

class RingBuffer {
  private lines: string[] = [];

  push(line: string): void {
    this.lines.push(line);
    if (this.lines.length > RING_BUFFER_CAPACITY) {
      this.lines.shift();
    }
  }

  snapshot(): string[] {
    return this.lines.slice();
  }

  clear(): void {
    this.lines = [];
  }
}

// ── Port utilities ───────────────────────────────────────────────────

function findFreePort(): Promise<number> {
  return new Promise((resolve, reject) => {
    const server = net.createServer();
    server.unref();
    server.on("error", reject);
    server.listen(0, "127.0.0.1", () => {
      const addr = server.address();
      if (!addr || typeof addr === "string") {
        server.close();
        reject(new Error("failed to resolve server address"));
        return;
      }
      const port = addr.port;
      server.close(() => resolve(port));
    });
  });
}

// ── Default commands per sidecar ─────────────────────────────────────

function commandForSidecar(id: SidecarId, port: number): { cmd: string; args: string[] } {
  switch (id) {
    case "jupyter":
      return {
        cmd: "jupyter",
        args: ["lab", "--no-browser", `--port=${port}`, "--ServerApp.token=''"],
      };
    case "tensorboard":
      return {
        cmd: "tensorboard",
        args: ["--logdir=.", `--port=${port}`, "--bind_all"],
      };
    case "mlflow-ui":
      return {
        cmd: "mlflow",
        args: ["ui", `--port=${port}`],
      };
    case "duckdb":
      return {
        cmd: "duckdb",
        args: ["-httpd", `-httpd_port=${port}`],
      };
  }
}

// ── SidecarManager ───────────────────────────────────────────────────

interface ManagedSidecar {
  process: ChildProcess;
  state: SidecarState;
  logs: RingBuffer;
}

export class SidecarManager {
  private sidecars = new Map<SidecarId, ManagedSidecar>();

  async start(spec: SidecarSpec): Promise<SidecarState> {
    // If already running, return current state.
    const existing = this.sidecars.get(spec.id);
    if (existing && (existing.state.status === "running" || existing.state.status === "starting")) {
      return existing.state;
    }

    const port = spec.port ?? await findFreePort();
    const { cmd, args } = commandForSidecar(spec.id, port);

    const state: SidecarState = {
      id: spec.id,
      status: "starting",
      port,
      url: `http://localhost:${port}`,
      error: null,
    };

    const logs = new RingBuffer();

    const env: NodeJS.ProcessEnv = {
      ...process.env,
      ...(spec.env ?? {}),
    };

    const child = spawn(cmd, args, {
      cwd: spec.cwd,
      env,
      stdio: ["ignore", "pipe", "pipe"],
    });

    const managed: ManagedSidecar = { process: child, state, logs };
    this.sidecars.set(spec.id, managed);

    child.stdout?.on("data", (chunk: Buffer) => {
      const lines = chunk.toString("utf8").split("\n");
      for (const line of lines) {
        if (line) logs.push(line);
      }
    });

    child.stderr?.on("data", (chunk: Buffer) => {
      const lines = chunk.toString("utf8").split("\n");
      for (const line of lines) {
        if (line) logs.push(line);
      }
    });

    child.on("spawn", () => {
      managed.state = { ...managed.state, status: "running" };
    });

    child.on("error", (err) => {
      managed.state = {
        ...managed.state,
        status: "error",
        error: err.message,
      };
    });

    child.on("exit", (code, signal) => {
      // Only transition to stopped/error if we haven't already been
      // cleaned up by an explicit stop() call.
      if (managed.state.status === "running" || managed.state.status === "starting") {
        if (code === 0 || code === null) {
          managed.state = { ...managed.state, status: "stopped", error: null };
        } else {
          managed.state = {
            ...managed.state,
            status: "error",
            error: `exited with code ${code}${signal ? ` (signal: ${signal})` : ""}`,
          };
        }
      }
    });

    return managed.state;
  }

  async stop(id: SidecarId): Promise<void> {
    const managed = this.sidecars.get(id);
    if (!managed) return;

    managed.state = { ...managed.state, status: "stopped", error: null };

    const child = managed.process;
    if (child.exitCode === null && !child.killed) {
      child.kill("SIGTERM");

      // Give the process a grace period before force-killing.
      await new Promise<void>((resolve) => {
        const timeout = setTimeout(() => {
          if (child.exitCode === null && !child.killed) {
            child.kill("SIGKILL");
          }
          resolve();
        }, 5_000);

        child.on("exit", () => {
          clearTimeout(timeout);
          resolve();
        });
      });
    }

    this.sidecars.delete(id);
  }

  status(id: SidecarId): SidecarState {
    const managed = this.sidecars.get(id);
    if (!managed) {
      return { id, status: "stopped", port: null, url: null, error: null };
    }
    return managed.state;
  }

  statusAll(): SidecarState[] {
    const ids: SidecarId[] = ["jupyter", "tensorboard", "mlflow-ui", "duckdb"];
    return ids.map((id) => this.status(id));
  }

  logs(id: SidecarId): string[] {
    const managed = this.sidecars.get(id);
    if (!managed) return [];
    return managed.logs.snapshot();
  }

  async stopAll(): Promise<void> {
    const ids = Array.from(this.sidecars.keys());
    await Promise.all(ids.map((id) => this.stop(id)));
  }
}

// ── IPC registration ─────────────────────────────────────────────────

/**
 * Registers sidecar IPC handlers on the main process. Call this from
 * main.ts during app setup.
 */
export function registerSidecarIpc(manager: SidecarManager): void {
  ipcMain.handle("sidecar:start", async (_evt, spec: SidecarSpec) => {
    return manager.start(spec);
  });

  ipcMain.handle("sidecar:stop", async (_evt, id: SidecarId) => {
    await manager.stop(id);
    return { ok: true };
  });

  ipcMain.handle("sidecar:status", async (_evt, id?: SidecarId) => {
    if (id) return manager.status(id);
    return manager.statusAll();
  });

  ipcMain.handle("sidecar:logs", async (_evt, id: SidecarId) => {
    return manager.logs(id);
  });
}
