import { afterEach, describe, expect, it, vi } from "vitest";
import type { Terminal } from "@xterm/xterm";
import { attachWebgl } from "../src/lib/terminal-font";

vi.mock("@xterm/addon-webgl", () => ({
  WebglAddon: class {
    onContextLoss = vi.fn();
  },
}));

afterEach(() => {
  vi.unstubAllGlobals();
});

describe("terminal renderer font support", () => {
  it.each([true, false])("uses WebGL only when canvas accepts the atlas font syntax (supported: %s)", supportsLeadingSpace => {
    let font = "10px sans-serif";
    const context = {
      get font() { return font; },
      set font(value: string) {
        if (!supportsLeadingSpace && /^\s/.test(value)) return;
        font = value.trim();
      },
    };
    vi.stubGlobal("navigator", { platform: "MacIntel", userAgent: "Electron" });
    vi.stubGlobal("document", { createElement: () => ({ getContext: () => context }) });
    const loadAddon = vi.fn();
    const terminal = { loadAddon, refresh: vi.fn(), rows: 24 } as unknown as Terminal;
    const state = { addon: null, disabledAfterContextLoss: false };

    attachWebgl(terminal, state);

    expect(loadAddon).toHaveBeenCalledTimes(supportsLeadingSpace ? 1 : 0);
  });
});
