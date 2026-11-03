/**
 * Jupyter Kernel Manager — implements the Jupyter Messaging Protocol
 * over ZeroMQ for per-cell code execution.
 *
 * Architecture:
 *   1. Discover kernels via `jupyter kernelspec list --json`
 *   2. Launch a kernel via `jupyter kernel --kernel=<name>`
 *   3. Connect to the kernel's ZeroMQ sockets (shell, iopub, stdin, control)
 *   4. Send execute_request messages, receive outputs via iopub
 *   5. Route outputs back to the renderer via IPC
 *
 * Wire protocol: each message is a multipart ZeroMQ frame sequence:
 *   [ident, DELIM, HMAC, header, parent_header, metadata, content, ...]
 * where DELIM = "<IDS|MSG>" and HMAC is computed over header+parent+meta+content.
 */

import { spawn, ChildProcess } from "node:child_process";
import * as fs from "node:fs";
import * as path from "node:path";
import * as crypto from "node:crypto";

function uuidv4(): string {
  return crypto.randomUUID();
}

// ZeroMQ is loaded dynamically to avoid issues if the native module
// fails to load (e.g. in dev without rebuild).
let zmq: typeof import("zeromq") | null = null;
async function getZmq() {
  if (!zmq) {
    try {
      zmq = await import("zeromq");
    } catch (e) {
      throw new Error(`ZeroMQ not available: ${e instanceof Error ? e.message : String(e)}`);
    }
  }
  return zmq;
}

/**
 * Find the jupyter binary. In packaged Electron apps, the PATH
 * doesn't include the user's shell profile paths. Search common
 * locations.
 */
function findJupyter(): string {
  const candidates = [
    "jupyter", // hope it's in PATH
    "/usr/local/bin/jupyter",
    "/opt/homebrew/bin/jupyter",
    `${process.env.HOME}/.local/bin/jupyter`,
    `${process.env.HOME}/miniconda3/bin/jupyter`,
    `${process.env.HOME}/anaconda3/bin/jupyter`,
    `${process.env.HOME}/.conda/bin/jupyter`,
  ];
  for (const c of candidates) {
    try {
      const { execSync } = require("node:child_process") as typeof import("node:child_process");
      execSync(`${c} --version`, { stdio: "ignore" });
      return c;
    } catch { /* not found */ }
  }
  return "jupyter"; // fallback
}

const JUPYTER_BIN = findJupyter();

const DELIM = "<IDS|MSG>";

interface ConnectionInfo {
  shell_port: number;
  iopub_port: number;
  stdin_port: number;
  control_port: number;
  hb_port: number;
  ip: string;
  key: string;
  transport: string;
  signature_scheme: string;
  kernel_name: string;
}

interface JupyterMessage {
  header: {
    msg_id: string;
    session: string;
    username: string;
    date: string;
    msg_type: string;
    version: string;
  };
  parent_header: Record<string, unknown>;
  metadata: Record<string, unknown>;
  content: Record<string, unknown>;
}

export interface KernelSpec {
  name: string;
  displayName: string;
  language: string;
}

export interface KernelOutput {
  msgType: string;
  content: Record<string, unknown>;
  parentMsgId: string;
}

export class JupyterKernel {
  private process: ChildProcess | null = null;
  private connectionFile: string | null = null;
  private connInfo: ConnectionInfo | null = null;
  private session: string = uuidv4();
  private shellSocket: any = null;
  private iopubSocket: any = null;
  private controlSocket: any = null;
  private onOutput: ((output: KernelOutput) => void) | null = null;
  private onStatus: ((status: string) => void) | null = null;
  private alive = false;
  private jupyterBin: string;

  constructor(
    private kernelName: string,
    private cwd: string,
    jupyterPath?: string,
  ) {
    this.jupyterBin = jupyterPath || JUPYTER_BIN;
  }

  setOutputHandler(handler: (output: KernelOutput) => void) {
    this.onOutput = handler;
  }

  setStatusHandler(handler: (status: string) => void) {
    this.onStatus = handler;
  }

  async start(): Promise<void> {
    // Launch kernel process.
    const proc = spawn(this.jupyterBin, ["kernel", `--kernel=${this.kernelName}`], {
      cwd: this.cwd,
      stdio: ["pipe", "pipe", "pipe"],
      env: { ...process.env },
    });
    this.process = proc;

    // The kernel prints the connection file path to stdout.
    const connFilePath = await new Promise<string>((resolve, reject) => {
      let stdout = "";
      const timeout = setTimeout(() => reject(new Error("Kernel startup timed out")), 30000);
      proc.stdout?.on("data", (data: Buffer) => {
        stdout += data.toString();
        // Look for the connection file path in output.
        const match = stdout.match(/Connection file: (.+\.json)/);
        if (match) {
          clearTimeout(timeout);
          resolve(match[1].trim());
        }
        // Alternative: jupyter kernel outputs JSON with connection_file.
        try {
          const parsed = JSON.parse(stdout.trim());
          if (parsed.connection_file) {
            clearTimeout(timeout);
            resolve(parsed.connection_file);
          }
        } catch { /* not JSON yet */ }
      });
      proc.stderr?.on("data", (data: Buffer) => {
        const text = data.toString();
        // Some kernels print the connection file to stderr.
        const match = text.match(/kernel-[\w-]+\.json/);
        if (match) {
          // Construct full path from jupyter runtime dir.
          const runtimeDir = path.join(
            process.env.HOME || "/tmp",
            "Library/Jupyter/runtime",
          );
          const candidate = path.join(runtimeDir, match[0]);
          if (fs.existsSync(candidate)) {
            clearTimeout(timeout);
            resolve(candidate);
          }
        }
      });
      proc.on("error", (err) => { clearTimeout(timeout); reject(err); });
      proc.on("exit", (code) => {
        clearTimeout(timeout);
        if (!this.alive) reject(new Error(`Kernel exited with code ${code}`));
      });
    });

    this.connectionFile = connFilePath;

    // Wait a moment for the file to be written.
    await new Promise((r) => setTimeout(r, 500));

    // Read connection info.
    const raw = fs.readFileSync(connFilePath, "utf8");
    this.connInfo = JSON.parse(raw) as ConnectionInfo;
    this.alive = true;

    // Connect ZeroMQ sockets.
    const z = await getZmq();
    const addr = (port: number) =>
      `${this.connInfo!.transport}://${this.connInfo!.ip}:${port}`;

    // Shell socket (DEALER) — for execute_request/reply.
    this.shellSocket = new z.Dealer();
    this.shellSocket.routingId = Buffer.from(uuidv4());
    await this.shellSocket.connect(addr(this.connInfo.shell_port));

    // IOPub socket (SUB) — for broadcast outputs.
    this.iopubSocket = new z.Subscriber();
    this.iopubSocket.subscribe("");
    await this.iopubSocket.connect(addr(this.connInfo.iopub_port));

    // Control socket (DEALER) — for interrupt/shutdown.
    this.controlSocket = new z.Dealer();
    this.controlSocket.routingId = Buffer.from(uuidv4());
    await this.controlSocket.connect(addr(this.connInfo.control_port));

    // Start listening on iopub.
    this.listenIopub();
  }

  private async listenIopub() {
    if (!this.iopubSocket) return;
    try {
      for await (const frames of this.iopubSocket) {
        if (!this.alive) break;
        const msg = this.decodeMessage(frames as Buffer[]);
        if (!msg) continue;

        const msgType = msg.header.msg_type;
        const parentMsgId = (msg.parent_header as any)?.msg_id ?? "";

        if (msgType === "status") {
          const state = (msg.content as any).execution_state;
          this.onStatus?.(state);
        }

        this.onOutput?.({
          msgType,
          content: msg.content,
          parentMsgId,
        });
      }
    } catch {
      // Socket closed.
    }
  }

  async execute(code: string): Promise<string> {
    if (!this.shellSocket || !this.connInfo) {
      throw new Error("Kernel not connected");
    }

    const msgId = uuidv4();
    const msg = this.buildMessage("execute_request", {
      code,
      silent: false,
      store_history: true,
      user_expressions: {},
      allow_stdin: false,
      stop_on_error: true,
    }, msgId);

    const frames = this.encodeMessage(msg);
    await this.shellSocket.send(frames);

    return msgId;
  }

  async interrupt(): Promise<void> {
    if (this.process) {
      this.process.kill("SIGINT");
    }
  }

  async restart(): Promise<void> {
    await this.shutdown();
    await new Promise((r) => setTimeout(r, 1000));
    await this.start();
  }

  async shutdown(): Promise<void> {
    this.alive = false;
    try {
      if (this.controlSocket && this.connInfo) {
        const msg = this.buildMessage("shutdown_request", { restart: false }, uuidv4());
        const frames = this.encodeMessage(msg);
        await this.controlSocket.send(frames);
      }
    } catch { /* best effort */ }

    try { this.shellSocket?.close(); } catch {}
    try { this.iopubSocket?.close(); } catch {}
    try { this.controlSocket?.close(); } catch {}
    this.shellSocket = null;
    this.iopubSocket = null;
    this.controlSocket = null;

    if (this.process) {
      this.process.kill();
      this.process = null;
    }

    // Clean up connection file.
    if (this.connectionFile && fs.existsSync(this.connectionFile)) {
      try { fs.unlinkSync(this.connectionFile); } catch {}
    }
  }

  isAlive(): boolean {
    return this.alive;
  }

  private buildMessage(
    msgType: string,
    content: Record<string, unknown>,
    msgId: string,
  ): JupyterMessage {
    return {
      header: {
        msg_id: msgId,
        session: this.session,
        username: "crab-desktop",
        date: new Date().toISOString(),
        msg_type: msgType,
        version: "5.3",
      },
      parent_header: {},
      metadata: {},
      content,
    };
  }

  private encodeMessage(msg: JupyterMessage): Buffer[] {
    const header = Buffer.from(JSON.stringify(msg.header));
    const parentHeader = Buffer.from(JSON.stringify(msg.parent_header));
    const metadata = Buffer.from(JSON.stringify(msg.metadata));
    const content = Buffer.from(JSON.stringify(msg.content));

    const key = this.connInfo?.key ?? "";
    let hmac = "";
    if (key) {
      const h = crypto.createHmac("sha256", key);
      h.update(header);
      h.update(parentHeader);
      h.update(metadata);
      h.update(content);
      hmac = h.digest("hex");
    }

    return [
      Buffer.from(DELIM),
      Buffer.from(hmac),
      header,
      parentHeader,
      metadata,
      content,
    ];
  }

  private decodeMessage(frames: Buffer[]): JupyterMessage | null {
    // Find the DELIM frame.
    let delimIdx = -1;
    for (let i = 0; i < frames.length; i++) {
      if (frames[i].toString() === DELIM) {
        delimIdx = i;
        break;
      }
    }
    if (delimIdx === -1 || delimIdx + 5 >= frames.length) return null;

    // frames[delimIdx+1] = HMAC (skip verification for now)
    const header = JSON.parse(frames[delimIdx + 2].toString());
    const parentHeader = JSON.parse(frames[delimIdx + 3].toString());
    const metadata = JSON.parse(frames[delimIdx + 4].toString());
    const content = JSON.parse(frames[delimIdx + 5].toString());

    return { header, parent_header: parentHeader, metadata, content };
  }
}

/**
 * List available Jupyter kernel specs.
 */
export async function listKernelSpecs(jupyterPath?: string): Promise<KernelSpec[]> {
  const bin = jupyterPath || JUPYTER_BIN;
  return new Promise((resolve) => {
    const proc = spawn(bin, ["kernelspec", "list", "--json"], {
      stdio: ["pipe", "pipe", "pipe"],
    });
    let stdout = "";
    proc.stdout?.on("data", (d: Buffer) => { stdout += d.toString(); });
    proc.on("error", () => resolve([]));
    proc.on("exit", () => {
      try {
        const data = JSON.parse(stdout);
        const specs: KernelSpec[] = [];
        for (const [name, spec] of Object.entries(data.kernelspecs ?? {})) {
          const s = spec as any;
          specs.push({
            name,
            displayName: s.spec?.display_name ?? name,
            language: s.spec?.language ?? "unknown",
          });
        }
        resolve(specs);
      } catch {
        resolve([]);
      }
    });
  });
}
