import { EventEmitter } from "node:events";
import { randomUUID } from "node:crypto";
import { Readable, Writable } from "node:stream";
import { finished } from "node:stream/promises";
import type { AgentProcessFactory, OwnedAgentProcess } from "./agent-process";
import type { RpcRouter } from "./ssh-router";

const CHUNK_BYTES = 16 * 1024;
type Stream = "stdout" | "stderr";
type Notification = { profileId: string; method: string; params: unknown };

function readExit(frame: object): { code: number | null; signal: number | null } {
  if (!("code" in frame) || !(frame.code === null || typeof frame.code === "number")
    || !("signal" in frame) || !(frame.signal === null || typeof frame.signal === "number")) throw new Error("Invalid native close receipt");
  return { code: frame.code, signal: frame.signal };
}

class NativeOutput extends Readable {
  private sequence = 0;
  private pending?: number;
  constructor(private readonly acknowledge: (sequence: number) => void) { super({ highWaterMark: CHUNK_BYTES }); }
  override _read(): void {
    if (this.pending === undefined) return;
    const sequence = this.pending;
    this.pending = undefined;
    this.acknowledge(sequence);
  }
  receive(bytes: Buffer, sequence: number): void {
    if (sequence !== this.sequence + 1 || this.pending !== undefined || bytes.length > CHUNK_BYTES) throw new Error("Native output frame is out of order or exceeds its credit");
    this.sequence = sequence;
    if (this.push(bytes)) this.acknowledge(sequence);
    else this.pending = sequence;
  }
}

/** Owns one native process on one exact SSH channel, including closure receipts. */
class RemoteNativeProcess extends EventEmitter {
  readonly stdout: NativeOutput;
  readonly stderr: NativeOutput;
  readonly stdin: Writable;
  readonly closed: Promise<void>;
  private readonly id = randomUUID();
  private readonly owner: object;
  private readonly ready: Promise<void>;
  private resolveClosed!: () => void;
  private rejectClosed!: (error: Error) => void;
  private ended = false;
  private stopping?: Promise<void>;

  constructor(private readonly router: RpcRouter, private readonly profileId: string, binary: string, args: string[], cwd: string) {
    super();
    const owner = router.remoteChannelOwner(profileId);
    if (!owner) throw new Error("Native process requires an attached SSH agent");
    this.owner = owner;
    this.closed = new Promise((resolve, reject) => { this.resolveClosed = resolve; this.rejectClosed = reject; });
    void this.closed.catch(() => {});
    const acknowledge = (stream: Stream, sequence: number) => {
      void this.request("native_process_ack", { stream, sequence }).catch((error: unknown) => this.fail(error));
    };
    this.stdout = new NativeOutput(sequence => acknowledge("stdout", sequence));
    this.stderr = new NativeOutput(sequence => acknowledge("stderr", sequence));
    this.stdin = new Writable({
      highWaterMark: CHUNK_BYTES,
      write: (bytes: Buffer, _encoding, complete) => {
        void this.write(bytes).then(() => complete(), (error: Error) => complete(error));
      },
      final: complete => { void this.ready.then(() => this.request("native_process_close_input")).then(() => complete(), (error: Error) => complete(error)); },
    });
    router.on("notification", this.notification);
    router.on("remote-channel-retired", this.retired);
    // Only command arguments cross the wire. Native credentials and PATH come
    // from the server login; local process.env and per-agent secrets stay local.
    this.ready = this.request("native_process_open", { executable: binary, args, cwd }).then(result => {
      if (!result || typeof result !== "object" || !("opened" in result) || result.opened !== true) throw new Error("Native process open was not acknowledged");
    });
    void this.ready.catch((error: unknown) => this.fail(error));
  }

  private request(method: string, params: Record<string, unknown> = {}): Promise<unknown> {
    return this.router.requestOnRemoteChannel(this.profileId, this.owner, method, { process_id: this.id, ...params });
  }

  private async write(bytes: Buffer): Promise<void> {
    await this.ready;
    if (this.ended || this.stopping) throw new Error("Native process input is closed");
    for (let offset = 0; offset < bytes.length; offset += CHUNK_BYTES) {
      const chunk = bytes.subarray(offset, offset + CHUNK_BYTES);
      const bytes_b64 = chunk.toString("base64");
      const result = await this.request("native_process_write", { bytes_b64 });
      if (!result || typeof result !== "object" || !("written" in result) || result.written !== chunk.length) throw new Error("Native process input delivery was not acknowledged");
    }
  }

  private readonly notification = (event: Notification): void => {
    if (event.profileId !== this.profileId || this.ended || this.router.remoteChannelOwner(this.profileId) !== this.owner) return;
    if (event.method !== "native_process/output" && event.method !== "native_process/closed") return;
    const frame = event.params;
    if (!frame || typeof frame !== "object" || !("process_id" in frame) || frame.process_id !== this.id) return;
    try {
      if (event.method === "native_process/closed") {
        if ("error" in frame) { this.finish(new Error(String(frame.error))); return; }
        const exit = readExit(frame);
        this.finish(undefined, exit.code, exit.signal);
        return;
      }
      if (!("stream" in frame) || !(frame.stream === "stdout" || frame.stream === "stderr")
        || !("bytes_b64" in frame) || typeof frame.bytes_b64 !== "string"
        || !("sequence" in frame) || !Number.isSafeInteger(frame.sequence)) throw new Error("Invalid native output frame");
      const bytes = Buffer.from(frame.bytes_b64, "base64");
      if (bytes.toString("base64") !== frame.bytes_b64) throw new Error("Invalid native output bytes");
      this[frame.stream].receive(bytes, frame.sequence as number);
    } catch (error: unknown) { this.fail(error); }
  };

  private readonly retired = (event: { profileId: string; owner: object; reason: Error }): void => {
    if (event.profileId !== this.profileId || event.owner !== this.owner) return;
    this.finish(new Error("SSH native process closure is unconfirmed", { cause: event.reason }));
  };

  kill(signal?: NodeJS.Signals | number): boolean {
    if (this.ended) return false;
    // Cancellation drains local readers so output credit cannot prevent the
    // physical stop receipt. The server owns TERM/KILL and descendant closure.
    this.stdout.resume(); this.stderr.resume();
    const force = signal === "SIGKILL" || signal === 9;
    if (this.stopping && force) {
      void this.ready.then(() => this.request("native_process_stop", { force: true })).catch((error: unknown) => this.finish(error instanceof Error ? error : new Error(String(error))));
    }
    this.stopping ??= this.ready.then(async () => {
      const result = await this.request("native_process_stop", { force });
      if (!result || typeof result !== "object" || !("closed" in result) || result.closed !== true) throw new Error("Native process closure was not acknowledged");
      const exit = readExit(result);
      this.finish(undefined, exit.code, exit.signal);
    });
    void this.stopping.catch((error: unknown) => this.finish(error instanceof Error ? error : new Error(String(error))));
    return true;
  }

  private fail(reason: unknown): void {
    if (this.ended) return;
    const error = reason instanceof Error ? reason : new Error(String(reason));
    this.emit("error", error);
    this.kill();
  }

  private finish(error?: Error, code: number | null = 1, signal: number | null = null): void {
    if (this.ended) return;
    this.ended = true;
    this.router.removeListener("notification", this.notification);
    this.router.removeListener("remote-channel-retired", this.retired);
    this.stdin.destroy(); this.stdout.push(null); this.stderr.push(null);
    // Node flushes a child's paused stdio after exit. Match that ordering so a
    // protocol reader closed during cancellation cannot strand the close event.
    this.stdout.resume(); this.stderr.resume();
    if (error) {
      this.rejectClosed(error);
      queueMicrotask(() => this.emit("error", error));
    }
    queueMicrotask(() => this.emit("exit", code, signal));
    void Promise.all([finished(this.stdout), finished(this.stderr)]).then(async () => {
      this.emit("close", code, signal);
      if (!error) {
        // A confirmed receipt makes release bookkeeping safe to abandon if
        // the transport retires afterward; server shutdown retires that state.
        await this.request("native_process_release").catch(() => {});
        this.resolveClosed();
      }
    }, (reason: unknown) => { this.rejectClosed(reason instanceof Error ? reason : new Error(String(reason))); });
  }
}

export function remoteAgentProcessFactory(router: RpcRouter, profileId: string): AgentProcessFactory {
  return (binary, args, options): OwnedAgentProcess => {
    const process = new RemoteNativeProcess(router, profileId, binary, args, options.cwd);
    return { process, closed: process.closed };
  };
}
