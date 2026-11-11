import { dialog, type BrowserWindow } from "electron";
import type { HostKeyInspection } from "./ssh-known-hosts";

/** Requests host-key approval in a parented native dialog owned by the connection. */
export async function confirmSshHostKey(parent: BrowserWindow, request: HostKeyInspection, signal: AbortSignal): Promise<boolean> {
  signal.throwIfAborted();
  if (parent.isDestroyed()) return false;
  const lifetime = new AbortController();
  const cancel = () => lifetime.abort();
  signal.addEventListener("abort", cancel, { once: true });
  parent.once("closed", cancel);
  const changed = request.status === "changed";
  const address = request.port === 22 ? request.hostname : `[${request.hostname}]:${request.port}`;
  try {
    const result = await dialog.showMessageBox(parent, {
      type: changed ? "warning" : "question",
      title: changed ? "SSH host key changed" : "Verify SSH host key",
      message: changed ? `The host key for ${address} has changed.` : `Trust the SSH host key for ${address}?`,
      detail: `${changed ? "Verify this change with the server owner before continuing.\n\nPreviously trusted fingerprints:\n" + request.previousFingerprints.join("\n") + "\n\n" : "Verify this fingerprint with the server owner before continuing.\n\n"}${request.keyType}\n${request.fingerprint}`,
      buttons: ["Reject", "Trust key"], defaultId: 0, cancelId: 0,
      checkboxLabel: changed ? "I verified that this key change is expected" : undefined,
      checkboxChecked: false, signal: lifetime.signal,
    });
    return !signal.aborted && !lifetime.signal.aborted && result.response === 1 && (!changed || result.checkboxChecked);
  } finally { signal.removeEventListener("abort", cancel); parent.removeListener("closed", cancel); }
}
