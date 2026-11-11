import type { Client, ConnectConfig } from "ssh2";
import { HostKeyError, type HostKeyVerifier } from "./ssh-known-hosts";

/** Authenticates one transport with revocable host trust and a network-only deadline. */
export function connectVerified(client: Client, config: ConnectConfig, hostname: string, port: number, lifetime: AbortController, verify: HostKeyVerifier, onHostKey?: (key: Buffer) => void): Promise<void> {
    const { signal } = lifetime;
    return new Promise((resolve, reject) => {
      let pending = true;
      let remaining = config.readyTimeout ?? 10_000;
      const bounded = remaining > 0;
      let started = 0;
      let timer: NodeJS.Timeout | undefined;
      const pause = () => {
        if (timer === undefined) return;
        clearTimeout(timer); timer = undefined;
        remaining -= Date.now() - started;
      };
      const resume = () => {
        if (!pending || signal.aborted || !bounded) return;
        started = Date.now();
        timer = setTimeout(() => { failed(new Error("SSH connection handshake timed out")); client.destroy(); }, Math.max(0, remaining));
      };
      const cleanup = () => {
        pending = false; pause();
        client.removeListener("ready", ready); client.removeListener("end", closed); client.removeListener("close", closed);
        signal.removeEventListener("abort", cancel);
      };
      const failed = (reason: unknown) => { cleanup(); reject(reason); };
      const ready = () => {
        // Match OpenSSH for exec/SFTP as well as PTYs. Small SSH frames must
        // not wait for delayed ACKs on every deployment or RPC round trip.
        client.setNoDelay(true);
        cleanup(); resolve();
      };
      const closed = () => failed(new Error("SSH connection closed before ready"));
      const cancel = () => { failed(signal.reason); client.destroy(); };
      client.once("ready", ready); client.once("end", closed); client.once("close", closed);
      // Keep an error listener until physical close, including on retired jump
      // clients. A late error must settle its owner without becoming unhandled.
      client.on("error", failed);
      client.once("close", () => client.removeListener("error", failed));
      signal.addEventListener("abort", cancel, { once: true });
      if (signal.aborted) { cancel(); return; }
      resume();
      try {
        client.connect({
          ...config, readyTimeout: 0,
          // ssh2's deadline includes user interaction. Only the network handshake
          // consumes our budget; disconnect/replacement owns an unanswered prompt.
          hostVerifier: (key: Buffer, callback: (accepted: boolean) => void) => {
            pause();
            void verify(hostname, port, key, signal).then(() => {
              if (signal.aborted) return;
              onHostKey?.(key);
              resume(); callback(true);
            }, (cause: unknown) => {
              if (signal.aborted) return;
              const reason = cause instanceof HostKeyError ? cause : new HostKeyError("SSH_HOST_TRUST_FAILED", "SSH host key verification failed", { cause });
              lifetime.abort(reason); callback(false);
            });
          },
        });
      } catch (reason: unknown) { failed(reason); }
    });
  }
