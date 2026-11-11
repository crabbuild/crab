import type { App } from "electron";
import { EventEmitter } from "node:events";
import { describe, expect, it, vi } from "vitest";
import { registerAppShutdown } from "../electron/app-shutdown";

describe("Electron app shutdown", () => {
  it.each([false, true])("awaits every owner and prevents repeated quit even when a sibling fails: %s", async fail => {
    const app = Object.assign(new EventEmitter(), { exit: vi.fn() });
    const prevented = vi.fn();
    let release!: () => void;
    const pending = vi.fn(() => new Promise<void>(resolve => { release = resolve; }));
    const finalized = vi.fn();
    const sibling = vi.fn(() => { if (fail) throw new Error("Owned shutdown failure"); });
    const error = vi.spyOn(console, "error").mockImplementation(() => {});
    try {
      registerAppShutdown(app as unknown as Pick<App, "on" | "exit">, [sibling, pending], finalized);
      app.emit("will-quit", { preventDefault: prevented });
      await vi.waitFor(() => expect(pending).toHaveBeenCalledOnce());
      app.emit("will-quit", { preventDefault: prevented });
      expect({ prevented: prevented.mock.calls.length, exits: app.exit.mock.calls, owners: [sibling.mock.calls.length, pending.mock.calls.length] }).toEqual({ prevented: 2, exits: [], owners: [1, 1] });
      expect(finalized).not.toHaveBeenCalled();
      release();
      await vi.waitFor(() => expect(app.exit.mock.calls).toEqual([[fail ? 1 : 0]]));
      expect(finalized).toHaveBeenCalledOnce();
      expect(error.mock.calls.length).toBe(fail ? 1 : 0);
    } finally { release?.(); error.mockRestore(); }
  });
});
