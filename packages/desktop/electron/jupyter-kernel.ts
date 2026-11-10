import { execFile, spawn, type ChildProcessWithoutNullStreams } from "node:child_process";
import { randomUUID } from "node:crypto";
import * as path from "node:path";
import { StringDecoder } from "node:string_decoder";
import { promisify } from "node:util";
import { app } from "electron";
import { getUserShellBaseEnv } from "./env/userShellEnv";
import type { JupyterRuntime, KernelExecution, KernelOutput, KernelSpec } from "./jupyter-types";

const exec = promisify(execFile);
// Python cannot read Electron's virtual ASAR filesystem. The packaged
// worker is explicitly unpacked; development uses the adjacent build asset.
const runtimeScript = app?.isPackaged
  ? path.join(process.resourcesPath, "app.asar.unpacked", "dist-electron", "jupyter-runtime.py")
  : path.join(__dirname, "jupyter-runtime.py");
const runtimeCommands = new Set<{ cancel: () => void; done: Promise<unknown> }>();
let commandsClosed = false;
type KernelAdmission = { env: NodeJS.ProcessEnv } & ({ cwd: string } | { serverUrl: string });

function runPython(pythonPath: string, args: string[], timeout: number, installs = false): Promise<{ stdout: string; stderr: string }> {
  const abort = new AbortController();
  const done = (async () => {
    if (commandsClosed) throw new Error("Jupyter services are shutting down");
    let cancel!: () => void;
    const cancelled = new Promise<never>((_resolve, reject) => {
      cancel = () => reject(new Error("Jupyter services are shutting down"));
      abort.signal.addEventListener("abort", cancel, { once: true });
    });
    let env: NodeJS.ProcessEnv;
    try { env = await Promise.race([getUserShellBaseEnv(), cancelled]); }
    finally { abort.signal.removeEventListener("abort", cancel); }
    if (commandsClosed) throw new Error("Jupyter services are shutting down");
    // Read-only probes can stop immediately. An admitted pip mutation keeps
    // ownership through completion; quitting must not abandon its environment.
    return exec(pythonPath, args, { env, timeout, maxBuffer: 1024 * 1024, signal: installs ? undefined : abort.signal });
  })();
  const entry = { cancel: () => abort.abort(), done };
  runtimeCommands.add(entry);
  void done.finally(() => runtimeCommands.delete(entry)).catch(() => {});
  return done;
}

export async function stopJupyterRuntimeCommands(): Promise<void> {
  commandsClosed = true;
  for (const command of runtimeCommands) command.cancel();
  await Promise.allSettled([...runtimeCommands].map(command => command.done));
}

export async function checkJupyterRuntime(pythonPath = process.platform === "win32" ? "python" : "python3"): Promise<JupyterRuntime> {
  try {
    const { stdout } = await runPython(pythonPath, ["-c", "import importlib.util,json,sys; print(json.dumps({'pythonPath':sys.executable,'available':importlib.util.find_spec('jupyter_client') is not None}))"], 10_000);
    return JSON.parse(stdout) as JupyterRuntime;
  } catch (error) {
    return { pythonPath: null, available: false, error: error instanceof Error ? error.message : String(error) };
  }
}

export async function listKernelSpecs(pythonPath: string): Promise<KernelSpec[]> {
  const { stdout } = await runPython(pythonPath, ["-u", runtimeScript, "--list"], 10_000);
  return JSON.parse(stdout) as KernelSpec[];
}

export async function installJupyter(pythonPath: string): Promise<string> {
  const { stdout, stderr } = await runPython(pythonPath, ["-m", "pip", "install", "jupyter_client", "ipykernel"], 120_000, true);
  return stdout + stderr;
}

export class JupyterKernel {
  private process: ChildProcessWithoutNullStreams | null = null;
  private closed: Promise<void> | null = null;
  private stopping: Promise<void> | null = null;
  private exitError: Error | null = null;
  private pending = new Map<string, { resolve: (result: unknown) => void; reject: (error: Error) => void }>();
  private onOutput: ((output: KernelOutput) => void) | null = null;
  private onStatus: ((status: string, error?: string) => void) | null = null;
  private workerFailure: Error | null = null;
  private alive = false;
  private started = false;
  private admission = new AbortController();

  constructor(private kernelName: string, private pythonPath: string, private prepare: (signal: AbortSignal) => Promise<() => KernelAdmission>) {}

  setOutputHandler(handler: (output: KernelOutput) => void) { this.onOutput = handler; }
  setStatusHandler(handler: (status: string, error?: string) => void) { this.onStatus = handler; }

  async start(): Promise<void> {
    if (this.started || this.stopping) throw new Error("Kernel already started or disconnected");
    this.started = true;
    const signal = this.admission.signal;
    let cancel!: () => void;
    const cancelled = new Promise<never>((_resolve, reject) => {
      cancel = () => reject(signal.reason);
      signal.addEventListener("abort", cancel, { once: true });
    });
    let admit: () => KernelAdmission;
    try { admit = await Promise.race([this.prepare(signal), cancelled]); }
    finally { signal.removeEventListener("abort", cancel); }
    signal.throwIfAborted();
    // Final native admission and worker creation share one synchronous turn;
    // a preparation promise cannot authorize a replacement repository session.
    const { env, ...location } = admit();
    const proc = spawn(this.pythonPath, ["-u", runtimeScript], { env, stdio: ["pipe", "pipe", "pipe"] });
    this.process = proc;
    proc.stdin.on("error", error => this.fail(error));
    let stderr = "", text = "";
    const decoder = new StringDecoder("utf8");
    proc.stderr.on("data", (bytes: Buffer) => { stderr = (stderr + bytes.toString()).slice(-16_384); });
    proc.stdout.on("data", (bytes: Buffer) => {
      text += decoder.write(bytes);
      let end: number;
      while ((end = text.indexOf("\n")) !== -1) {
        const line = text.slice(0, end); text = text.slice(end + 1);
        try { this.receive(JSON.parse(line) as Record<string, unknown>); }
        catch (error) { this.fail(error instanceof Error ? error : new Error(String(error))); }
      }
    });
    this.closed = new Promise(resolve => {
      proc.once("error", error => this.fail(error));
      proc.once("close", (code, signal) => {
        this.process = null; this.alive = false;
        const failure = this.workerFailure ?? new Error(`Kernel worker exited (${signal ?? code})${stderr ? `: ${stderr.trim()}` : ""}`);
        if (code !== 0) this.exitError = failure;
        this.fail(failure);
        this.onStatus?.(this.stopping ? "disconnected" : "dead", failure.message);
        resolve();
      });
    });
    try {
      await this.request("start", { kernelName: this.kernelName, ...location });
      if (this.stopping) throw new Error("Kernel disconnected during startup");
      this.alive = true;
    } catch (error) {
      try { await this.shutdown(); }
      catch (cleanup) { throw new AggregateError([error, cleanup], `Kernel startup failed: ${String(error)}; cleanup failed: ${String(cleanup)}`); }
      throw error;
    }
  }

  async execute(code: string, requestId: string, cell: number): Promise<KernelExecution> {
    if (!this.alive) throw new Error("Kernel not connected");
    return await this.request("execute", { code, cell }, requestId) as KernelExecution;
  }

  async interrupt(): Promise<void> { await this.request("interrupt"); }
  async restart(): Promise<void> {
    try {
      await this.request("restart");
    } catch (error) {
      await this.shutdown();
      throw error;
    }
  }

  shutdown(): Promise<void> {
    if (!this.stopping) this.stopping = (async () => {
      this.alive = false;
      this.admission.abort(new Error("Kernel disconnected during startup"));
      // EOF is the worker's shutdown admission, even if startup is pending.
      // Await its exit so KernelManager still owns child/connection cleanup.
      this.process?.stdin.end();
      await this.closed;
      if (this.exitError) throw this.exitError;
    })();
    return this.stopping;
  }

  isAlive(): boolean { return this.alive; }

  private request(method: string, params = {}, id: string = randomUUID()): Promise<unknown> {
    const proc = this.process;
    if (!proc || this.stopping) return Promise.reject(new Error("Kernel not connected"));
    if (this.pending.has(id)) return Promise.reject(new Error("Duplicate kernel request"));
    return new Promise((resolve, reject) => {
      this.pending.set(id, { resolve, reject });
      proc.stdin.write(JSON.stringify({ id, method, params }) + "\n", error => { if (error) { this.pending.delete(id); reject(error); } });
    });
  }

  private receive(message: Record<string, unknown>): void {
    if (message.kind === "reply" && typeof message.id === "string") {
      const request = this.pending.get(message.id); this.pending.delete(message.id);
      if (typeof message.error === "string") request?.reject(new Error(message.error));
      else request?.resolve(message.result);
    } else if (message.kind === "output") {
      if (typeof message.requestId !== "string" || typeof message.msgType !== "string" || !message.content || typeof message.content !== "object") throw new Error("Invalid kernel output");
      this.onOutput?.({ requestId: message.requestId, msgType: message.msgType, content: message.content as Record<string, unknown> });
    } else if (message.kind === "status" && typeof message.status === "string") this.onStatus?.(message.status);
    else if (message.kind === "failed") {
      this.workerFailure = new Error(String(message.error));
      this.fail(this.workerFailure);
    }
    else throw new Error("Invalid kernel worker message");
  }

  private fail(error: Error): void {
    for (const request of this.pending.values()) request.reject(error);
    this.pending.clear();
  }
}
