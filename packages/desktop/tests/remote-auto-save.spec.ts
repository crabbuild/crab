import { afterEach, describe, it, expect, vi } from "vitest";
import { createElement } from "react";
import { renderToString } from "react-dom/server";
import { getPendingSaveCount, useRemoteAutoSave, type RemoteAutoSaveState } from "../src/hooks/useRemoteAutoSave";

const transport = vi.hoisted(() => ({ write: vi.fn() }));
vi.mock("../src/ipc", () => ({ rpc: { writeText: transport.write } }));
vi.mock("../src/hooks/useRepoCapabilities", () => ({ useRepoCapabilities: () => ({ canEdit: true }) }));

function deferred() {
  let resolve!: () => void, reject!: (error: Error) => void;
  const promise = new Promise<void>((yes, no) => { resolve = yes; reject = no; });
  return { promise, resolve, reject };
}

// Render the actual hook to obtain its handlers and React-owned refs. These
// cases exercise asynchronous save acknowledgements, not effect scheduling.
function handlers(dirty: (key: string, value: boolean) => void): RemoteAutoSaveState {
  let result!: RemoteAutoSaveState;
  function Buffer() { result = useRemoteAutoSave("save-fixture", "note.txt", "tab", dirty); return null; }
  renderToString(createElement(Buffer));
  return result;
}

afterEach(() => { vi.useRealTimers(); transport.write.mockReset(); });

describe("useRemoteAutoSave", () => {
  describe("getPendingSaveCount", () => {
    it("returns 0 when no saves are pending", () => {
      // The module-level pendingSaves map starts empty.
      expect(getPendingSaveCount()).toBe(0);
    });
  });

  it("an older acknowledgement cannot mark a newer buffer clean", async () => {
    vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout"] });
    const first = deferred(), next = deferred(), dirty = vi.fn();
    transport.write.mockReturnValueOnce(first.promise).mockReturnValueOnce(next.promise);
    const save = handlers(dirty);
    save.handleSave("first");
    save.handleChange("latest");
    first.resolve(); await new Promise<void>(resolve => setImmediate(resolve));
    expect(dirty).not.toHaveBeenCalledWith("tab", false);
    save.handleSave("latest");
    next.resolve(); await new Promise<void>(resolve => setImmediate(resolve));
    expect(dirty).toHaveBeenLastCalledWith("tab", false);
  });

  it("failed saves remain pending until an explicit retry succeeds", async () => {
    vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout"] });
    const first = deferred(), next = deferred();
    transport.write.mockReturnValueOnce(first.promise).mockReturnValueOnce(next.promise);
    const save = handlers(vi.fn());
    save.handleSave("first"); save.handleChange("latest");
    first.reject(new Error("Fixture disconnected")); await new Promise<void>(resolve => setImmediate(resolve));
    expect(getPendingSaveCount()).toBe(1);
    save.handleSave("latest");
    expect(transport.write.mock.calls.at(-1)).toEqual(["save-fixture", "note.txt", "latest"]);
    next.resolve(); await new Promise<void>(resolve => setImmediate(resolve));
    expect(getPendingSaveCount()).toBe(0);
  });
});
