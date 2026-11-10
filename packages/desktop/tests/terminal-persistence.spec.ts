import { describe, it, expect } from "vitest";
import {
  serializeLayout,
  deserializeLayout,
  type TerminalViewState,
  type PersistedLayout,
  type SplitArrangement,
} from "../src/lib/terminal-persistence";

describe("terminal-persistence", () => {
  describe("serializeLayout", () => {
    it("serializes a basic single-tab layout", () => {
      const state: TerminalViewState = {
        tabs: [
          { id: "term-1", title: "zsh 1", shell: "/bin/zsh", cwd: "/home/user" },
        ],
        splits: [],
        activeTabIndex: 0,
      };

      const result = serializeLayout(state);

      expect(result.version).toBe(1);
      expect(result.tabs).toHaveLength(1);
      expect(result.tabs[0].id).toBe("term-1");
      expect(result.tabs[0].shell).toBe("/bin/zsh");
      expect(result.tabs[0].cwd).toBe("/home/user");
      expect(result.tabs[0].title).toBe("zsh 1");
      expect(result.activeTabIndex).toBe(0);
      expect(result.splits).toEqual([]);
      expect(result.quickTerminal).toBeUndefined();
    });

    it("serializes multiple tabs with correct active index", () => {
      const state: TerminalViewState = {
        tabs: [
          { id: "term-1", title: "zsh 1", shell: "/bin/zsh", cwd: "/home" },
          { id: "term-2", title: "bash 2", shell: "/bin/bash", cwd: "/tmp" },
          { id: "term-3", title: "fish 3", shell: "/usr/bin/fish", cwd: "/var" },
        ],
        splits: [],
        activeTabIndex: 2,
      };

      const result = serializeLayout(state);

      expect(result.tabs).toHaveLength(3);
      expect(result.activeTabIndex).toBe(2);
    });

    it("clamps activeTabIndex to valid range", () => {
      const state: TerminalViewState = {
        tabs: [
          { id: "term-1", title: "zsh", shell: "/bin/zsh", cwd: "/" },
        ],
        splits: [],
        activeTabIndex: 99,
      };

      const result = serializeLayout(state);
      expect(result.activeTabIndex).toBe(0);
    });

    it("includes env and startupCommand when present", () => {
      const state: TerminalViewState = {
        tabs: [
          {
            id: "term-1",
            title: "claude",
            shell: "/bin/zsh",
            cwd: "/project",
            env: { CLAUDE_API_KEY: "sk-123" },
            startupCommand: "claude",
          },
        ],
        splits: [],
        activeTabIndex: 0,
      };

      const result = serializeLayout(state);

      expect(result.tabs[0].env).toEqual({ CLAUDE_API_KEY: "sk-123" });
      expect(result.tabs[0].startupCommand).toBe("claude");
    });

    it("omits env when empty object", () => {
      const state: TerminalViewState = {
        tabs: [
          { id: "term-1", title: "zsh", shell: "/bin/zsh", cwd: "/", env: {} },
        ],
        splits: [],
        activeTabIndex: 0,
      };

      const result = serializeLayout(state);
      expect(result.tabs[0].env).toBeUndefined();
    });

    it("serializes quickTerminal when present", () => {
      const state: TerminalViewState = {
        tabs: [
          { id: "term-1", title: "zsh", shell: "/bin/zsh", cwd: "/" },
        ],
        splits: [],
        activeTabIndex: 0,
        quickTerminal: {
          id: "qt-1",
          title: "Quick Terminal",
          shell: "/bin/zsh",
          cwd: "/home/user",
        },
      };

      const result = serializeLayout(state);

      expect(result.quickTerminal).toBeDefined();
      expect(result.quickTerminal!.id).toBe("qt-1");
      expect(result.quickTerminal!.title).toBe("Quick Terminal");
    });

    it("serializes split arrangements", () => {
      const splits: SplitArrangement[] = [
        {
          direction: "horizontal",
          sizes: [50, 50],
          children: ["term-1", "term-2"],
        },
      ];

      const state: TerminalViewState = {
        tabs: [
          { id: "term-1", title: "left", shell: "/bin/zsh", cwd: "/" },
          { id: "term-2", title: "right", shell: "/bin/zsh", cwd: "/" },
        ],
        splits,
        activeTabIndex: 0,
      };

      const result = serializeLayout(state);
      expect(result.splits).toEqual(splits);
    });
  });

  describe("deserializeLayout", () => {
    it("deserializes a valid layout", () => {
      const data: PersistedLayout = {
        version: 1,
        tabs: [
          { id: "term-1", shell: "/bin/zsh", cwd: "/home", title: "zsh 1" },
        ],
        splits: [],
        activeTabIndex: 0,
      };

      const result = deserializeLayout(data);

      expect(result).not.toBeNull();
      expect(result!.version).toBe(1);
      expect(result!.tabs).toHaveLength(1);
      expect(result!.tabs[0].id).toBe("term-1");
      expect(result!.activeTabIndex).toBe(0);
    });

    it("returns null for null input", () => {
      expect(deserializeLayout(null)).toBeNull();
    });

    it("returns null for undefined input", () => {
      expect(deserializeLayout(undefined)).toBeNull();
    });

    it("returns null for non-object input", () => {
      expect(deserializeLayout("string")).toBeNull();
      expect(deserializeLayout(42)).toBeNull();
      expect(deserializeLayout(true)).toBeNull();
    });

    it("returns null for wrong version", () => {
      const data = {
        version: 2,
        tabs: [{ id: "t", shell: "sh", cwd: "/", title: "t" }],
        splits: [],
        activeTabIndex: 0,
      };
      expect(deserializeLayout(data)).toBeNull();
    });

    it("returns null for missing version", () => {
      const data = {
        tabs: [{ id: "t", shell: "sh", cwd: "/", title: "t" }],
        splits: [],
        activeTabIndex: 0,
      };
      expect(deserializeLayout(data)).toBeNull();
    });

    it("preserves an intentionally empty layout", () => {
      const data = {
        version: 1,
        tabs: [],
        splits: [],
        activeTabIndex: 0,
      };
      expect(deserializeLayout(data)?.tabs).toEqual(data.tabs);
    });

    it("restores tabs across multiple panes beyond the per-pane limit", () => {
      const tabs = Array.from({ length: 13 }, (_, i) => ({
        id: `t-${i}`,
        shell: "/bin/zsh",
        cwd: "/",
        title: `tab ${i}`,
      }));
      const data = {
        version: 1,
        tabs,
        splits: [],
        activeTabIndex: 0,
      };
      expect(deserializeLayout(data)?.tabs).toEqual(data.tabs);
    });

    it("returns null for invalid tab (missing id)", () => {
      const data = {
        version: 1,
        tabs: [{ shell: "/bin/zsh", cwd: "/", title: "t" }],
        splits: [],
        activeTabIndex: 0,
      };
      expect(deserializeLayout(data)).toBeNull();
    });

    it("returns null for invalid tab (empty id)", () => {
      const data = {
        version: 1,
        tabs: [{ id: "", shell: "/bin/zsh", cwd: "/", title: "t" }],
        splits: [],
        activeTabIndex: 0,
      };
      expect(deserializeLayout(data)).toBeNull();
    });

    it("returns null for invalid tab (non-string env value)", () => {
      const data = {
        version: 1,
        tabs: [{ id: "t", shell: "sh", cwd: "/", title: "t", env: { key: 123 } }],
        splits: [],
        activeTabIndex: 0,
      };
      expect(deserializeLayout(data)).toBeNull();
    });

    it("clamps activeTabIndex to valid range", () => {
      const data = {
        version: 1,
        tabs: [
          { id: "t-1", shell: "/bin/zsh", cwd: "/", title: "tab 1" },
          { id: "t-2", shell: "/bin/zsh", cwd: "/", title: "tab 2" },
        ],
        splits: [],
        activeTabIndex: 10,
      };

      const result = deserializeLayout(data);
      expect(result).not.toBeNull();
      expect(result!.activeTabIndex).toBe(1);
    });

    it("deserializes split arrangements", () => {
      const data = {
        version: 1,
        tabs: [
          { id: "t-1", shell: "/bin/zsh", cwd: "/", title: "left" },
          { id: "t-2", shell: "/bin/zsh", cwd: "/", title: "right" },
        ],
        splits: [
          {
            direction: "horizontal",
            sizes: [50, 50],
            children: ["t-1", "t-2"],
          },
        ],
        activeTabIndex: 0,
      };

      const result = deserializeLayout(data);
      expect(result).not.toBeNull();
      expect(result!.splits).toHaveLength(1);
      expect(result!.splits[0].direction).toBe("horizontal");
      expect(result!.splits[0].sizes).toEqual([50, 50]);
      expect(result!.splits[0].children).toEqual(["t-1", "t-2"]);
    });

    it("deserializes nested split arrangements", () => {
      const data = {
        version: 1,
        tabs: [
          { id: "t-1", shell: "/bin/zsh", cwd: "/", title: "a" },
          { id: "t-2", shell: "/bin/zsh", cwd: "/", title: "b" },
          { id: "t-3", shell: "/bin/zsh", cwd: "/", title: "c" },
        ],
        splits: [
          {
            direction: "horizontal",
            sizes: [50, 50],
            children: [
              "t-1",
              {
                direction: "vertical",
                sizes: [50, 50],
                children: ["t-2", "t-3"],
              },
            ],
          },
        ],
        activeTabIndex: 0,
      };

      const result = deserializeLayout(data);
      expect(result).not.toBeNull();
      expect(result!.splits[0].children[0]).toBe("t-1");
      const nested = result!.splits[0].children[1] as SplitArrangement;
      expect(nested.direction).toBe("vertical");
      expect(nested.children).toEqual(["t-2", "t-3"]);
    });

    it("returns null for invalid split direction", () => {
      const data = {
        version: 1,
        tabs: [{ id: "t", shell: "sh", cwd: "/", title: "t" }],
        splits: [{ direction: "diagonal", sizes: [100], children: ["t"] }],
        activeTabIndex: 0,
      };
      expect(deserializeLayout(data)).toBeNull();
    });

    it("returns null for mismatched sizes and children length", () => {
      const data = {
        version: 1,
        tabs: [{ id: "t", shell: "sh", cwd: "/", title: "t" }],
        splits: [{ direction: "horizontal", sizes: [50, 50], children: ["t"] }],
        activeTabIndex: 0,
      };
      expect(deserializeLayout(data)).toBeNull();
    });

    it("deserializes quickTerminal when present", () => {
      const data = {
        version: 1,
        tabs: [{ id: "t-1", shell: "/bin/zsh", cwd: "/", title: "main" }],
        splits: [],
        activeTabIndex: 0,
        quickTerminal: {
          id: "qt-1",
          shell: "/bin/zsh",
          cwd: "/home",
          title: "Quick Terminal",
        },
      };

      const result = deserializeLayout(data);
      expect(result).not.toBeNull();
      expect(result!.quickTerminal).toBeDefined();
      expect(result!.quickTerminal!.id).toBe("qt-1");
    });

    it("returns null for invalid quickTerminal", () => {
      const data = {
        version: 1,
        tabs: [{ id: "t-1", shell: "/bin/zsh", cwd: "/", title: "main" }],
        splits: [],
        activeTabIndex: 0,
        quickTerminal: { id: "", shell: "sh", cwd: "/", title: "qt" },
      };
      expect(deserializeLayout(data)).toBeNull();
    });

    it("handles optional fields in sessions", () => {
      const data = {
        version: 1,
        tabs: [
          {
            id: "t-1",
            shell: "/bin/zsh",
            cwd: "/project",
            title: "dev",
            env: { NODE_ENV: "development" },
            startupCommand: "npm run dev",
            scrollbackPath: "/tmp/scrollback/t-1.bin",
          },
        ],
        splits: [],
        activeTabIndex: 0,
      };

      const result = deserializeLayout(data);
      expect(result).not.toBeNull();
      expect(result!.tabs[0].env).toEqual({ NODE_ENV: "development" });
      expect(result!.tabs[0].startupCommand).toBe("npm run dev");
      expect(result!.tabs[0].scrollbackPath).toBe("/tmp/scrollback/t-1.bin");
    });
  });

  describe("round-trip", () => {
    it("serialize then deserialize produces equivalent layout", () => {
      const state: TerminalViewState = {
        tabs: [
          { id: "term-1", title: "zsh 1", shell: "/bin/zsh", cwd: "/home/user" },
          { id: "term-2", title: "bash 2", shell: "/bin/bash", cwd: "/tmp" },
        ],
        splits: [
          {
            direction: "horizontal",
            sizes: [60, 40],
            children: ["term-1", "term-2"],
          },
        ],
        activeTabIndex: 1,
        quickTerminal: {
          id: "qt-1",
          title: "Quick",
          shell: "/bin/zsh",
          cwd: "/home",
        },
      };

      const serialized = serializeLayout(state);
      const json = JSON.parse(JSON.stringify(serialized));
      const deserialized = deserializeLayout(json);

      expect(deserialized).not.toBeNull();
      expect(deserialized!.version).toBe(serialized.version);
      expect(deserialized!.tabs).toEqual(serialized.tabs);
      expect(deserialized!.splits).toEqual(serialized.splits);
      expect(deserialized!.activeTabIndex).toBe(serialized.activeTabIndex);
      expect(deserialized!.quickTerminal).toEqual(serialized.quickTerminal);
    });

    it("round-trip with env and startupCommand", () => {
      const state: TerminalViewState = {
        tabs: [
          {
            id: "t-1",
            title: "agent",
            shell: "/bin/zsh",
            cwd: "/project",
            env: { API_KEY: "secret", DEBUG: "true" },
            startupCommand: "claude --model opus",
          },
        ],
        splits: [],
        activeTabIndex: 0,
      };

      const serialized = serializeLayout(state);
      const json = JSON.parse(JSON.stringify(serialized));
      const deserialized = deserializeLayout(json);

      expect(deserialized).not.toBeNull();
      expect(deserialized!.tabs[0].env).toEqual({ API_KEY: "secret", DEBUG: "true" });
      expect(deserialized!.tabs[0].startupCommand).toBe("claude --model opus");
    });
  });
});
