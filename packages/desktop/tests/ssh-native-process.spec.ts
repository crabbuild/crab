import { EventEmitter } from "node:events";
import { describe, expect, it, vi } from "vitest";
import type { ClientChannel } from "ssh2";
import type { SshSessionManager } from "../electron/ssh-manager";
import { RpcRouter } from "../electron/ssh-router";
import { remoteAgentProcessFactory } from "../electron/ssh-native-process";

interface Request { id: number; method: string; params: Record<string, unknown> }

class Channel extends EventEmitter {
  requests: Request[] = [];
  bytes: Buffer[] = [];
  holdStop = false;
  write(frame: string, written?: (error?: Error | null) => void): boolean {
    const request = JSON.parse(frame) as Request;
    this.requests.push(request);
    written?.();
    let result: object = {};
    if (request.method === "native_process_open") result = { opened: true };
    if (request.method === "native_process_write") {
      const bytes = Buffer.from(String(request.params.bytes_b64), "base64");
      this.bytes.push(bytes); result = { written: bytes.length };
    }
    if (request.method === "native_process_stop") {
      if (this.holdStop) return true;
      result = { closed: true, code: 0, signal: null };
    }
    this.reply(request, result);
    return true;
  }
  reply(request: Request, result: unknown): void { this.emit("data", Buffer.from(JSON.stringify({ jsonrpc: "2.0", id: request.id, result }) + "\n")); }
  output(stream: "stdout" | "stderr", bytes: Buffer, sequence: number): void {
    const process_id = this.requests[0].params.process_id;
    this.emit("data", Buffer.from(JSON.stringify({ jsonrpc: "2.0", method: "native_process/output", params: { process_id, stream, bytes_b64: bytes.toString("base64"), sequence } }) + "\n"));
  }
}

function fixture() {
  const manager = { getStatus: () => "connected", getConnection: () => ({ profile: {} }) } as unknown as SshSessionManager;
  const router = new RpcRouter(manager, { request: async () => { throw new Error("Unexpected local RPC"); } });
  const channel = new Channel();
  router.attachChannel("server", channel as unknown as ClientChannel);
  const factory = remoteAgentProcessFactory(router, "server");
  const owner = factory("native-cli", ["--stdio"], { cwd: "/owned/workspace", env: { LOCAL_ONLY_CREDENTIAL: "fixture-secret" } });
  const errors: Error[] = [];
  owner.process.on("error", (error: Error) => errors.push(error));
  owner.process.stdin?.on("error", () => {});
  return { router, channel, owner, errors };
}

describe("SSH native process ownership", () => {
  it("chunks stdin without changing bytes or transmitting local environment", async () => {
    const { channel, owner } = fixture();
    const bytes = Buffer.from("雪🙂\u0000".repeat(10_000));
    await new Promise<void>((resolve, reject) => owner.process.stdin!.write(bytes, error => error ? reject(error) : resolve()));
    owner.process.kill();
    await owner.closed;
    expect({ params: channel.requests[0].params, bytes: Buffer.concat(channel.bytes), bounded: channel.bytes.every(chunk => chunk.length <= 16 * 1024) }).toEqual({ params: { process_id: expect.any(String), executable: "native-cli", args: ["--stdio"], cwd: "/owned/workspace" }, bytes, bounded: true });
  });

  it("returns output credit only after the bounded reader consumes its frame", async () => {
    const { channel, owner } = fixture();
    const output = owner.process.stdout!;
    const bytes = Buffer.alloc(16 * 1024, 7);
    channel.output("stdout", bytes, 1);
    expect(channel.requests.filter(request => request.method === "native_process_ack")).toHaveLength(0);
    const received: Buffer[] = [];
    output.on("data", (chunk: Buffer) => received.push(chunk));
    await vi.waitFor(() => expect(channel.requests.filter(request => request.method === "native_process_ack")).toHaveLength(1));
    owner.process.kill();
    await owner.closed;
    expect(Buffer.concat(received)).toEqual(bytes);
  });

  it("waits for confirmed stop and does not require a second notification", async () => {
    const { channel, owner } = fixture();
    channel.holdStop = true;
    let closed = false;
    void owner.closed.then(() => { closed = true; });
    owner.process.kill();
    await vi.waitFor(() => expect(channel.requests.some(request => request.method === "native_process_stop")).toBe(true));
    expect(closed).toBe(false);
    const request = channel.requests.find(request => request.method === "native_process_stop")!;
    channel.reply(request, { closed: true, code: null, signal: 15 });
    await owner.closed;
    expect(closed).toBe(true);
  });

  it("rejects unconfirmed closure and fences every write after channel replacement", async () => {
    const { router, channel, owner, errors } = fixture();
    const replacement = new Channel();
    const closed = expect(owner.closed).rejects.toThrow("closure is unconfirmed");
    router.attachChannel("server", replacement as unknown as ClientChannel);
    await closed;
    channel.emit("close");
    owner.process.kill();
    await new Promise<void>(resolve => setImmediate(resolve));
    expect({ replacement: replacement.requests, errors: errors.map(error => error.message) }).toEqual({ replacement: [], errors: ["SSH native process closure is unconfirmed"] });
  });
});
