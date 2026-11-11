import { afterEach, beforeEach, expect, it, vi } from "vitest";

const native = vi.hoisted(() => ({
  write: vi.fn(async (_items: { data: Record<string, Blob> }[]) => {}),
  guest: {
    isDestroyed: vi.fn(() => false),
    on: vi.fn(),
    capturePage: vi.fn(async () => ({ toPNG: () => Buffer.from([137, 80, 78, 71]) })),
  },
}));

vi.mock("electron", () => ({
  app: { getPath: () => "/crab-clipboard-fixture" },
  session: {}, dialog: {}, BrowserWindow: {},
  webContents: { fromId: () => native.guest },
  clipboard: { write: native.write },
  ClipboardItem: class { constructor(readonly data: Record<string, Blob>) {} },
}));

import { browserManager } from "../electron/browser/browser-manager";

beforeEach(() => {
  native.write.mockReset().mockResolvedValue(undefined);
  browserManager.registerGuest({ browserTabId: "clipboard-fixture", worktreeId: "fixture", webContentsId: 1, rendererWebContentsId: 2 });
});
afterEach(() => { browserManager.unregisterGuest("clipboard-fixture"); vi.clearAllMocks(); });

it("acknowledges a screenshot only after the native PNG clipboard write completes", async () => {
  let acknowledge!: () => void;
  native.write.mockImplementationOnce(() => new Promise<void>(resolve => { acknowledge = resolve; }));
  let finished = false;
  const copying = browserManager.screenshotToClipboard("clipboard-fixture").then(result => { finished = true; return result; });
  try {
    await vi.waitFor(() => expect(native.write).toHaveBeenCalledOnce());
    const image = native.write.mock.calls[0][0][0].data["image/png"];
    expect({ type: image.type, bytes: Buffer.from(await image.arrayBuffer()), finished }).toEqual({
      type: "image/png", bytes: Buffer.from([137, 80, 78, 71]), finished: false,
    });
    acknowledge();
    await expect(copying).resolves.toBe(true);
  } finally { acknowledge?.(); await copying; }
});

it("reports a rejected native clipboard write as a failed screenshot", async () => {
  native.write.mockRejectedValueOnce(new Error("Clipboard unavailable"));
  await expect(browserManager.screenshotToClipboard("clipboard-fixture")).resolves.toBe(false);
});
