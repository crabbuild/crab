/**
 * AdoptedPtySubprocessHandle — SubprocessHandle over an inherited PTY master fd.
 *
 * Used by the upgrade successor to wrap PTY fds passed from the predecessor
 * via stdio. Reads use libuv's terminal stream so nonblocking descriptors wait
 * for readiness. The filesystem writer borrows the fd; the reader owns closure.
 *
 * kill() is neutralized after dispose() to prevent SIGHUP-to-recycled-PID.
 * forceKill() uses process.kill(pid, "SIGKILL") directly and is NOT
 * neutralized — callers must call forceKill BEFORE dispose when both are needed.
 */

import * as fs from "node:fs";
import { ReadStream } from "node:tty";
import type { SubprocessHandle } from "./types";
import type { AdoptedSession } from "./fd-handoff";

export class AdoptedPtySubprocessHandle implements SubprocessHandle {
  readonly pid: number;
  readonly fd: number;

  private readStream: ReadStream;
  private writeStream: fs.WriteStream;
  private disposed = false;
  private dataCallbacks: Array<(data: string) => void> = [];
  private exitCallbacks: Array<(exitCode: number) => void> = [];

  constructor(adopted: AdoptedSession) {
    this.pid = adopted.pid;
    this.fd = adopted.fd;

    // Filesystem readers treat EAGAIN on a live nonblocking PTY as an error.
    // Use the same readiness-driven reader as node-pty instead.
    this.readStream = new ReadStream(adopted.fd);
    this.readStream.setEncoding("utf8");

    this.writeStream = fs.createWriteStream("", {
      fd: adopted.fd,
      autoClose: false,
    } as Parameters<typeof fs.createWriteStream>[1]);

    this.readStream.on("data", (chunk: string | Buffer) => {
      if (this.disposed) return;
      const data = typeof chunk === "string" ? chunk : chunk.toString("utf-8");
      for (const cb of this.dataCallbacks) {
        cb(data);
      }
    });

    this.readStream.on("error", () => {
      // PTY master fd closed — treat as process exit
      if (!this.disposed) {
        for (const cb of this.exitCallbacks) {
          cb(0);
        }
      }
    });

    this.writeStream.on("error", () => {
      if (!this.disposed) {
        for (const cb of this.exitCallbacks) cb(1);
      }
    });

    this.readStream.on("end", () => {
      if (!this.disposed) {
        for (const cb of this.exitCallbacks) {
          cb(0);
        }
      }
    });
  }

  write(data: string): void {
    if (this.disposed) return;
    this.writeStream.write(data, "utf-8");
  }

  resize(_cols: number, _rows: number): void {
    // Resize is not supported for adopted fds — the predecessor already
    // owns the PTY dimensions. The successor will resize via the normal
    // resize request path once the session is live.
  }

  kill(signal?: string): void {
    // Neutralized after dispose to prevent SIGHUP-to-recycled-PID
    if (this.disposed) return;
    try {
      process.kill(this.pid, signal ?? "SIGTERM");
    } catch {
      // Process already exited — ignore ESRCH
    }
  }

  forceKill(): void {
    // Uses process.kill directly — NOT neutralized by dispose.
    // Callers MUST call forceKill BEFORE dispose when both are needed.
    try {
      process.kill(this.pid, "SIGKILL");
    } catch {
      // Process already exited — ignore ESRCH
    }
  }

  signal(sig: string): void {
    if (this.disposed) return;
    try {
      process.kill(this.pid, sig);
    } catch {
      // Process already exited — ignore ESRCH
    }
  }

  pause(): void {
    if (this.disposed) return;
    this.readStream.pause();
  }

  resume(): void {
    if (this.disposed) return;
    this.readStream.resume();
  }

  onData(cb: (data: string) => void): void {
    this.dataCallbacks.push(cb);
  }

  onExit(cb: (exitCode: number) => void): void {
    this.exitCallbacks.push(cb);
  }

  /**
   * Replay a buffer of pending output through the data callbacks.
   *
   * Called immediately after the Session wires its onData handler so the
   * pending buffer from the predecessor flows through the full data path
   * (headless emulator + client fan-out) without a gap.
   */
  replayBuffer(buffer: Buffer): void {
    if (buffer.length === 0) return;
    const data = buffer.toString("utf-8");
    for (const cb of this.dataCallbacks) {
      cb(data);
    }
  }

  /**
   * Release the adopted fd streams. After this call:
   * - kill() becomes a no-op (SIGHUP neutralization)
   * - write/resize/signal/pause/resume become no-ops
   * - Pending writes finish before the reader closes the descriptor
   */
  dispose(): void {
    if (this.disposed) return;
    this.disposed = true;
    this.dataCallbacks = [];
    this.exitCallbacks = [];
    // Explicit destroy closes even an autoClose:false filesystem stream.
    // Finish borrowed writes before the reader closes their shared descriptor.
    this.writeStream.end(() => this.readStream.destroy());
  }
}
