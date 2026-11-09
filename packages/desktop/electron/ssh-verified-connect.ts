import type { Client, ConnectConfig } from "ssh2";

export type VerifyHostKey = (hostname: string, port: number, key: Buffer, signal: AbortSignal) => Promise<boolean>;

/** Authenticate only after host verification; user decisions pause the deadline. */
export function connectVerified(client: Client, config: ConnectConfig, hostname: string, port: number, verify: VerifyHostKey): Promise<void> {
  return new Promise((resolve, reject) => {
    const controller = new AbortController();
    let authenticated = false;
    let settled = false;
    let remaining = config.readyTimeout ?? 10_000;
    let started = 0;
    let timer: ReturnType<typeof setTimeout> | undefined;
    const stop = (error: Error) => {
      clearTimeout(timer);
      controller.abort();
      client.removeListener("ready", onReady);
      client.removeListener("error", onError);
      client.removeListener("close", onClose);
      if (!settled) { settled = true; reject(error); }
    };
    const onReady = () => {
      authenticated = true; settled = true;
      clearTimeout(timer);
      resolve();
    };
    const onError = (error: Error) => stop(error);
    const onClose = () => stop(new Error("SSH connection closed before authentication"));
    const fail = (error: Error) => {
      if (controller.signal.aborted) return;
      // Rekey failures occur after this promise resolved. Preserve their cause
      // through the connection's error boundary before closing its channels.
      if (authenticated) client.emit("error", error); else stop(error);
      client.destroy();
    };
    const resume = () => {
      if (authenticated || remaining <= 0) return;
      started = Date.now();
      timer = setTimeout(() => fail(Object.assign(new Error("Timed out while waiting for SSH handshake"), { code: "ETIMEDOUT", level: "client-timeout" })), remaining);
    };
    client.once("ready", onReady);
    client.once("error", onError);
    client.once("close", onClose);
    resume();
    try {
      client.connect({
        ...config,
        // ssh2 includes asynchronous host verification in its ready timer.
        // Own that clock, but keep verification active for subsequent rekeys.
        readyTimeout: 0,
        hostVerifier: (key: Buffer, callback: (accepted: boolean) => void) => {
          if (!authenticated && timer) { clearTimeout(timer); remaining = Math.max(1, remaining - (Date.now() - started)); }
          void Promise.resolve().then(() => verify(hostname, port, key, controller.signal)).then(accepted => {
            if (controller.signal.aborted) return;
            resume(); callback(accepted);
          }, error => fail(error instanceof Error ? error : new Error(String(error))));
        },
      });
    } catch (error) { fail(error instanceof Error ? error : new Error(String(error))); }
  });
}
