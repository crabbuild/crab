import { EventEmitter } from "node:events";
import { describe, expect, it, vi } from "vitest";
import type { BrowserWindow, MessageBoxOptions, MessageBoxReturnValue } from "electron";
import { confirmSshHostKey } from "../electron/ssh-host-key-dialog";
import type { HostKeyInspection } from "../electron/ssh-known-hosts";
const { showMessageBox } = vi.hoisted(() => ({ showMessageBox: vi.fn() }));
vi.mock("electron", () => ({ dialog: { showMessageBox } }));
const request: HostKeyInspection = { hostname: "fixture.invalid", port: 2222, keyType: "ssh-rsa", fingerprint: "SHA256:fixture", previousFingerprints: ["SHA256:previous"], status: "changed" };
function windowOwner() {
  const window = Object.assign(new EventEmitter(), { isDestroyed: () => false });
  return { window, parent: window as unknown as BrowserWindow };
}
describe("native SSH host verification", () => {
  it.each([[0, false, false], [1, false, false], [1, true, true]])("requires both trust and changed-key acknowledgement (%s, %s)", async (response, checkboxChecked, accepted) => {
    const { parent } = windowOwner(); showMessageBox.mockResolvedValueOnce({ response, checkboxChecked });
    expect(await confirmSshHostKey(parent, request, new AbortController().signal)).toBe(accepted);
  });
  it("parents the prompt and defaults Enter and Escape to rejection", async () => {
    const { parent } = windowOwner(); showMessageBox.mockResolvedValueOnce({ response: 0, checkboxChecked: false });
    await confirmSshHostKey(parent, request, new AbortController().signal);
    expect(showMessageBox.mock.calls.at(-1)).toEqual([parent, expect.objectContaining({ defaultId: 0, cancelId: 0, detail: expect.stringContaining("SHA256:previous"), signal: expect.any(AbortSignal) })]);
  });
  it.each(["connection", "window"])("cancels the dialog when its %s closes", async owner => {
    const { window, parent } = windowOwner(); const connection = new AbortController(); let dialogSignal: AbortSignal | undefined;
    showMessageBox.mockImplementationOnce((_parent: BrowserWindow, options: MessageBoxOptions) => new Promise<MessageBoxReturnValue>(resolve => {
      dialogSignal = options.signal;
      options.signal!.addEventListener("abort", () => resolve({ response: 1, checkboxChecked: true }), { once: true });
    }));
    const pending = confirmSshHostKey(parent, request, connection.signal);
    if (owner === "window") window.emit("closed"); else connection.abort();
    expect({ accepted: await pending, aborted: dialogSignal?.aborted, listeners: window.listenerCount("closed") }).toEqual({ accepted: false, aborted: true, listeners: 0 });
  });
});
