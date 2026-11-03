import { useCallback, useEffect, useRef, useState } from "react";
import { Terminal } from "@xterm/xterm";
import { FitAddon } from "@xterm/addon-fit";
import { WebglAddon } from "@xterm/addon-webgl";
import { Unicode11Addon } from "@xterm/addon-unicode11";
import { WebLinksAddon } from "@xterm/addon-web-links";
import "@xterm/xterm/css/xterm.css";
import { Plus, Send, X, Terminal as TerminalIcon } from "lucide-react";
import { OrchestratorTask } from "./TaskCard";
import {
  registerTerminal,
  appendOutput,
  markTerminalClosed,
  removeTerminal,
  terminalTail,
} from "../../lib/terminal-registry";

// ── Types ───────────────────────────────────────────────────────────

interface Props {
  task: OrchestratorTask;
  /** The workspace directory path for this task's worktree. */
  workspacePath?: string;
  /**
   * Optional callback the parent wires up to drop the failing command's
   * output into the chat composer as agent feedback. When provided, a
   * "Send error" button appears whenever a non-zero exit is detected.
   */
  onSendErrorToAgent?: (feedback: string) => void;
}

interface TermTab {
  id: string;
  title: string;
}

interface TermInstance {
  term: Terminal;
  fitAddon: FitAddon;
}

// ── IPC (matches the native terminal integration in main.ts) ────────

const ptyIpc = {
  spawn: (cwd: string) =>
    (window as any).crab.termSpawn(cwd) as Promise<{ id: string }>,
  write: (id: string, data: string) =>
    (window as any).crab.termWrite(id, data) as Promise<void>,
  resize: (id: string, cols: number, rows: number) =>
    (window as any).crab.termResize(id, cols, rows) as Promise<void>,
  kill: (id: string) =>
    (window as any).crab.termKill(id) as Promise<void>,
  onData: (listener: (evt: { id: string; data: string }) => void) =>
    (window as any).crab.onTermData(listener) as () => void,
  onExit: (listener: (evt: { id: string; exitCode: number }) => void) =>
    (window as any).crab.onTermExit(listener) as () => void,
};

// ── Component ───────────────────────────────────────────────────────

/**
 * Multi-tab embedded terminal for the orchestrator workspace.
 * Uses the same node-pty + xterm.js infrastructure as the native
 * terminal view, including WebGL rendering and pre-warmed pool.
 */
export function TerminalTab({ task: _task, workspacePath, onSendErrorToAgent }: Props) {
  const [tabs, setTabs] = useState<TermTab[]>([]);
  const [activeTabId, setActiveTabId] = useState<string | null>(null);
  /**
   * Non-zero exit codes per terminal id, surfaced as a "Send error to
   * agent" chip next to the tab title. Cleared when the user either
   * dispatches it to the agent or explicitly dismisses it.
   */
  const [failedExits, setFailedExits] = useState<Map<string, { exitCode: number; at: number }>>(
    new Map(),
  );

  const instancesRef = useRef<Map<string, TermInstance>>(new Map());
  const containerRef = useRef<HTMLDivElement>(null);
  const tabCounter = useRef(0);
  const mountedRef = useRef(true);

  // Spawn a new shell tab.
  const spawnTab = useCallback(async () => {
    if (!workspacePath) return;
    try {
      const result = await ptyIpc.spawn(workspacePath);
      const id = result.id;
      tabCounter.current++;
      const title = `shell ${tabCounter.current}`;

      if (!mountedRef.current) {
        void ptyIpc.kill(id).catch(() => {});
        return;
      }

      // Register the session with the global registry so the @ context
      // picker can surface it.
      registerTerminal({
        id,
        label: `${_task.title ? _task.title + " · " : ""}${title}`,
        cwd: workspacePath,
      });

      setTabs((prev) => [...prev, { id, title }]);
      setActiveTabId(id);
    } catch (e) {
      console.error("[TerminalTab] Failed to spawn shell:", e);
    }
  }, [workspacePath]);

  // Close a tab.
  const closeTab = useCallback((tabId: string) => {
    void ptyIpc.kill(tabId).catch(() => {});

    const inst = instancesRef.current.get(tabId);
    if (inst) {
      inst.term.dispose();
      instancesRef.current.delete(tabId);
    }

    // Drop from the global registry so it stops appearing in the
    // @ context picker after the user explicitly closes it.
    removeTerminal(tabId);

    setTabs((prev) => {
      const remaining = prev.filter((t) => t.id !== tabId);
      // Switch to last remaining tab.
      setActiveTabId((current) => {
        if (current !== tabId) return current;
        return remaining[remaining.length - 1]?.id ?? null;
      });
      return remaining;
    });
  }, []);

  // Track whether we've spawned the initial shell.
  const initialSpawnedRef = useRef(false);

  // On mount: spawn initial shell only once the container is visible.
  // The terminal is kept mounted but hidden (display:none) when other tabs
  // are active. Spawning while hidden causes the first prompt to render at
  // the wrong column width (default 80) since fitAddon can't measure a
  // zero-size container. We use an IntersectionObserver to wait until the
  // container is actually visible before spawning.
  useEffect(() => {
    mountedRef.current = true;

    if (!workspacePath || !containerRef.current) return;

    const observer = new IntersectionObserver(
      (entries) => {
        const visible = entries.some((e) => e.isIntersecting);
        if (visible && !initialSpawnedRef.current) {
          initialSpawnedRef.current = true;
          void spawnTab();
        }
      },
      { threshold: 0.01 },
    );

    observer.observe(containerRef.current);

    return () => {
      mountedRef.current = false;
      observer.disconnect();
      for (const [id, inst] of instancesRef.current.entries()) {
        inst.term.dispose();
        void ptyIpc.kill(id).catch(() => {});
      }
      instancesRef.current.clear();
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [workspacePath]);

  // Listen for terminal data from all sessions.
  useEffect(() => {
    const unsubscribe = ptyIpc.onData(({ id, data }) => {
      const inst = instancesRef.current.get(id);
      if (inst) {
        // Strip ED 3 (\x1b[3J) to preserve scrollback on Ctrl+L.
        const sanitized = data.replace(/\x1b\[3J/g, "");
        inst.term.write(sanitized);
      }
      // Mirror into the registry buffer so the @ picker can attach recent output.
      appendOutput(id, data);
    });
    return unsubscribe;
  }, []);

  // Listen for terminal exit events.
  useEffect(() => {
    const unsubscribe = ptyIpc.onExit(({ id, exitCode }) => {
      const inst = instancesRef.current.get(id);
      if (inst) {
        inst.term.write(`\r\n\x1b[90m[Process exited with code ${exitCode}]\x1b[0m\r\n`);
      }
      markTerminalClosed(id);
      // Surface non-zero exits so the user can forward them to the agent.
      // The shell exits with the code of the last command it ran, so a
      // red exit here is a strong "something failed" signal.
      if (exitCode !== 0 && onSendErrorToAgent) {
        setFailedExits((prev) => {
          const next = new Map(prev);
          next.set(id, { exitCode, at: Date.now() });
          return next;
        });
      }
    });
    return unsubscribe;
  }, [onSendErrorToAgent]);

  // ─── "Send error to agent" ──────────────────────────────────────

  const sendErrorToAgent = useCallback(
    (tabId: string) => {
      const failure = failedExits.get(tabId);
      if (!failure || !onSendErrorToAgent) return;

      // Grab the most recent output from the registry — this matches
      // what the user actually sees on screen, ANSI-stripped for safety
      // so markdown rendering stays clean when the agent quotes it back.
      const rawTail = terminalTail(tabId, 16 * 1024);
      const stripped = stripAnsi(rawTail);
      const last200 = lastLines(stripped, 200);
      const tab = tabs.find((t) => t.id === tabId);
      const label = tab?.title ?? "terminal";

      const feedback = [
        `The last command in \`${label}\` failed with exit code ${failure.exitCode}.`,
        "Please investigate and fix the root cause. Output (last 200 lines):",
        "",
        "```",
        last200,
        "```",
      ].join("\n");

      onSendErrorToAgent(feedback);
      setFailedExits((prev) => {
        const next = new Map(prev);
        next.delete(tabId);
        return next;
      });
    },
    [failedExits, onSendErrorToAgent, tabs],
  );

  const dismissError = useCallback((tabId: string) => {
    setFailedExits((prev) => {
      const next = new Map(prev);
      next.delete(tabId);
      return next;
    });
  }, []);

  // Create/show/hide xterm instances when activeTabId changes.
  useEffect(() => {
    if (!containerRef.current || !activeTabId) return;
    const container = containerRef.current;

    // Show/hide existing terminals.
    for (const [id, inst] of instancesRef.current.entries()) {
      const el = inst.term.element?.parentElement;
      if (el) el.style.display = id === activeTabId ? "block" : "none";
    }

    // Create xterm instance for the active tab if it doesn't exist.
    if (!instancesRef.current.has(activeTabId)) {
      const term = new Terminal({
        cursorBlink: true,
        fontSize: 13,
        fontFamily: "ui-monospace, SFMono-Regular, Menlo, Monaco, monospace",
        theme: {
          background: "#0b0d12",
          foreground: "#e7eaf1",
          cursor: "#4f8cff",
          selectionBackground: "#2a3757",
          black: "#1a1e2e",
          red: "#f87171",
          green: "#4ade80",
          yellow: "#facc15",
          blue: "#60a5fa",
          magenta: "#c084fc",
          cyan: "#22d3ee",
          white: "#e7eaf1",
          brightBlack: "#475569",
          brightRed: "#fca5a5",
          brightGreen: "#86efac",
          brightYellow: "#fde047",
          brightBlue: "#93c5fd",
          brightMagenta: "#d8b4fe",
          brightCyan: "#67e8f9",
          brightWhite: "#f8fafc",
        },
        scrollback: 10000,
        allowProposedApi: true,
      });

      const fitAddon = new FitAddon();
      term.loadAddon(fitAddon);

      // Unicode 11 for proper emoji/CJK width.
      const unicode11 = new Unicode11Addon();
      term.loadAddon(unicode11);
      term.unicode.activeVersion = "11";

      // Clickable links.
      term.loadAddon(new WebLinksAddon());

      const wrapper = document.createElement("div");
      wrapper.style.height = "100%";
      wrapper.style.width = "100%";
      container.appendChild(wrapper);
      term.open(wrapper);

      // WebGL renderer for GPU-accelerated performance.
      requestAnimationFrame(() => {
        try {
          term.loadAddon(new WebglAddon());
        } catch {
          // Falls back to canvas renderer silently.
        }
      });

      // Fit after layout settles. Use a longer delay to ensure the
      // container has its final dimensions (especially when switching
      // from display:none → block via tab activation).
      const id = activeTabId;
      const doFit = () => {
        if (container.offsetWidth > 0 && container.offsetHeight > 0) {
          fitAddon.fit();
          void ptyIpc.resize(id, term.cols, term.rows);
        }
      };
      // First attempt after a microtask + rAF.
      setTimeout(() => requestAnimationFrame(doFit), 0);
      // Second attempt after layout has fully settled (covers display toggle).
      setTimeout(() => requestAnimationFrame(doFit), 100);

      // Wire user input → PTY.
      term.onData((data: string) => {
        void ptyIpc.write(id, data);
      });

      // Wire resize → PTY.
      term.onResize(({ cols, rows }: { cols: number; rows: number }) => {
        void ptyIpc.resize(id, cols, rows);
      });

      instancesRef.current.set(activeTabId, { term, fitAddon });
    } else {
      // Re-fit the active terminal in case container size changed.
      const inst = instancesRef.current.get(activeTabId);
      if (inst) {
        requestAnimationFrame(() => {
          if (container.offsetWidth > 0 && container.offsetHeight > 0) {
            inst.fitAddon.fit();
          }
        });
      }
    }
  }, [activeTabId, tabs]);

  // Handle container resize — fit the active terminal.
  // Also triggers when the terminal becomes visible (display: none → block).
  useEffect(() => {
    if (!containerRef.current) return;
    const container = containerRef.current;

    const resizeObserver = new ResizeObserver(() => {
      if (!activeTabId) return;
      const inst = instancesRef.current.get(activeTabId);
      if (inst && container.offsetWidth > 0 && container.offsetHeight > 0) {
        requestAnimationFrame(() => {
          inst.fitAddon.fit();
          // Notify the PTY of the new size so the shell redraws its prompt.
          void ptyIpc.resize(activeTabId, inst.term.cols, inst.term.rows);
        });
      }
    });

    resizeObserver.observe(container);

    // Also fit when the component becomes visible (parent display changes).
    const mutationObserver = new MutationObserver(() => {
      if (!activeTabId) return;
      const inst = instancesRef.current.get(activeTabId);
      if (inst && container.offsetWidth > 0 && container.offsetHeight > 0) {
        requestAnimationFrame(() => {
          inst.fitAddon.fit();
          void ptyIpc.resize(activeTabId, inst.term.cols, inst.term.rows);
        });
      }
    });

    // Observe the parent's style attribute for display changes.
    const parent = container.parentElement;
    if (parent) {
      mutationObserver.observe(parent, { attributes: true, attributeFilter: ["style"] });
    }

    return () => {
      resizeObserver.disconnect();
      mutationObserver.disconnect();
    };
  }, [activeTabId]);

  if (!workspacePath) {
    return (
      <div className="flex h-full items-center justify-center text-xs text-gx-muted">
        No workspace directory available.
      </div>
    );
  }

  return (
    <div className="flex h-full flex-col">
      {/* Tab bar */}
      <div className="flex shrink-0 items-center border-b border-gx-border bg-gx-surface">
        {tabs.map((tab) => (
          <div
            key={tab.id}
            className={`group flex items-center gap-1 border-r border-gx-border px-3 py-1.5 text-[11px] ${
              activeTabId === tab.id
                ? "bg-gx-bg-sub text-gx-text"
                : "text-gx-muted hover:bg-gx-surface-hover"
            }`}
          >
            <button
              type="button"
              onClick={() => setActiveTabId(tab.id)}
              className="flex items-center gap-1 truncate"
            >
              <TerminalIcon size={10} />
              {tab.title}
            </button>
            <button
              type="button"
              onClick={() => closeTab(tab.id)}
              className="opacity-0 transition group-hover:opacity-100 hover:text-gx-error"
              aria-label="Close terminal tab"
            >
              <X size={10} />
            </button>
          </div>
        ))}
        <button
          type="button"
          onClick={() => void spawnTab()}
          className="flex h-full items-center px-2 text-gx-muted hover:text-gx-text"
          title="New terminal tab"
          aria-label="New terminal tab"
        >
          <Plus size={12} />
        </button>

        {/* Per-tab failure banner */}
        {activeTabId && failedExits.has(activeTabId) && onSendErrorToAgent && (
          <div className="ml-auto flex items-center gap-1.5 pr-2 text-[10px]">
            <span className="text-gx-error">
              Exit {failedExits.get(activeTabId)!.exitCode}
            </span>
            <button
              type="button"
              onClick={() => sendErrorToAgent(activeTabId)}
              className="inline-flex items-center gap-1 rounded-md border border-gx-accent/40 bg-gx-accent/10 px-2 py-0.5 text-gx-accent hover:bg-gx-accent/20 transition"
              title="Send the last 200 lines to the agent and ask it to fix this"
            >
              <Send size={9} /> Send error to agent
            </button>
            <button
              type="button"
              onClick={() => dismissError(activeTabId)}
              className="rounded p-0.5 text-gx-muted hover:text-gx-text"
              aria-label="Dismiss exit notice"
            >
              <X size={10} />
            </button>
          </div>
        )}
      </div>

      {/* Terminal container */}
      <div ref={containerRef} className="min-h-0 flex-1 overflow-hidden bg-[#0b0d12] p-3" />
    </div>
  );
}

// ── Helpers ─────────────────────────────────────────────────────────

/** Strip ANSI escape codes — terminals emit colour and cursor codes that
 *  aren't useful (and sometimes render as markdown garbage) when the
 *  agent quotes the output back. */
function stripAnsi(str: string): string {
  // eslint-disable-next-line no-control-regex
  return str.replace(/\x1b\[[0-9;]*[a-zA-Z]/g, "").replace(/\x1b\][^\x07]*\x07/g, "");
}

/** Take the last `n` non-empty lines from a string, preserving order. */
function lastLines(str: string, n: number): string {
  const lines = str.split(/\r?\n/);
  return lines.slice(-n).join("\n");
}
