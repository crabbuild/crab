import React, { createContext, useCallback, useContext, useEffect, useMemo, useState } from "react";
import { mainProcess } from "./ipc";

// ---- Schema -----------------------------------------------------------

/**
 * Settings kept small and opinionated on purpose. Every knob here is
 * one that a non-technical user might reasonably want to change —
 * anything deeper (remote auth, proxies, storage layout) lives in
 * `~/.crab/config.toml` and stays out of the UI.
 *
 * Adding a new setting = one line in `defaults` + a UI row.
 */
export interface Settings {
  // Appearance
  theme: "dark" | "light" | "hc-dark" | "hc-light" | "system";
  editorFontSize: number; // 10..20
  editorWordWrap: boolean;
  editorMinimap: boolean;
  showActivityBar: boolean;

  // Editor theme (Shiki/TextMate theme ID for syntax highlighting)
  editorThemeDark: string;
  editorThemeLight: string;

  // Previews
  maxPreviewKib: number; // agent-enforced cap on read_text/read_bytes
  parquetPreviewRows: 50 | 100 | 200 | 500;

  // Workspace
  reopenReposOnStartup: boolean;
  defaultCloneDir: string | null;

  // Diff
  preferCrabDiff: boolean;

  // Changes view
  /** Show a confirm dialog before discarding a hunk / file when
   *  the discard touches more than this many lines. `0` disables
   *  the dialog entirely — every discard goes through without
   *  interruption. */
  discardConfirmThreshold: number;

  // Commit
  /** When enabled, the commit subject input is replaced by a
   *  structured conventional-commit helper (type / scope / breaking /
   *  description). The agent still receives the final composed string. */
  conventionalCommits: boolean;

  /** When enabled and staged files include `.ipynb` notebooks, the
   *  commit composer strips cell outputs before committing (staging-
   *  only transform — the working tree file is untouched). */
  stripNotebookOutputs: boolean;

  /** Size threshold (in MB) above which the commit composer warns
   *  about non-pointer-tracked files. Set to 0 to disable. */
  largeFileThresholdMb: number;

  // Memory
  /** Renderer heap cap in MB. When `performance.memory.usedJSHeapSize`
   *  exceeds this value, the least-recently-focused tab's state is
   *  evicted to OPFS. */
  heapCapMb: number;

  // Logs / diagnostics
  maxLogLines: 1000 | 2000 | 5000 | 10000;
  agentLogLevel: "off" | "info" | "debug" | "trace";
  openDevtoolsOnLaunch: boolean;

  // Notifications
  notifications: {
    muteJobCompletion: boolean;
    mutePrUpdates: boolean;
    muteAgentErrors: boolean;
    doNotDisturb: boolean;
    doNotDisturbUntil: number | null; // Unix timestamp ms, null = indefinite when DND is on
  };

  // Onboarding
  /** Whether the first-run welcome tour has been dismissed. */
  welcomeSeen: boolean;

  // AI commit message generation (L.5)
  /** Which AI provider to use for commit message suggestions. */
  aiProvider: "none" | "local-llama" | "openai-compatible";
  /** URL for the AI provider endpoint. */
  aiEndpoint: string | null;
  /** Model name (e.g., "llama3", "gpt-4"). */
  aiModel: string | null;

  // Feature flags (R-S2)
  /** Per-pillar feature flags. Default-on for stable pillars, default-off
   *  for in-development ones. Flags survive upgrade. */
  features: {
    /** Worktree manager (Pillar B). Behind a flag for internal dogfooding
     *  before general rollout. */
    worktrees: boolean;
    /** SSH remote connections. When disabled, the SSH view and related
     *  menu items are hidden from the UI. */
    sshRemoteEnabled: boolean;
  };

  // AI Assistant (L.7)
  /** Whether the contextual AI assistant is enabled. Default OFF — requires opt-in. */
  aiAssistantEnabled: boolean;

  // Orchestrator
  /** Whether the orchestrator mode (agent tasks) is enabled. Default OFF — requires opt-in. */
  orchestratorEnabled: boolean;
  /** Default agent CLI for new orchestrator tasks. */
  orchestratorDefaultAgent: string;
  /** Custom path to the Claude Code CLI binary (empty = use PATH). */
  orchestratorClaudeCodePath: string;
  /** Custom path to the Codex CLI binary (empty = use PATH). */
  orchestratorCodexPath: string;
  /** Custom binary path for a user-provided agent CLI. */
  orchestratorCustomBinaryPath: string;
  /** Display name for the custom agent binary. */
  orchestratorCustomBinaryName: string;
  /** Base directory for agent worktrees (empty = system temp). */
  orchestratorWorktreeBaseDir: string;
  /** Days to keep completed/discarded worktrees before auto-pruning. */
  orchestratorRetentionDays: number;
  /** Maximum number of concurrent agent tasks. */
  orchestratorMaxConcurrent: number;
  /** When enabled, passes the user's AI provider API key to the agent CLI via env var. */
  orchestratorApiKeyPassthrough: boolean;
  /** Setup script executed after worktree creation, before agent starts. */
  orchestratorSetupScript: string;
  /** Run script executed when user clicks "Run" in the workspace. */
  orchestratorRunScript: string;
  /** Archive script executed before worktree removal on archive/discard. */
  orchestratorArchiveScript: string;
  /** When enabled, only one workspace can have an active run process at a time. */
  orchestratorNonconcurrentRun: boolean;
  /** Start of the port range for auto-assigned workspace ports. */
  orchestratorPortRangeStart: number;
  /** End of the port range for auto-assigned workspace ports. */
  orchestratorPortRangeEnd: number;

  // Telemetry
  /** Opt-in anonymous telemetry. Default OFF. */
  telemetryOptIn: boolean;
  /** Whether the first-run telemetry prompt has been shown. */
  telemetryPromptSeen: boolean;

  // LSP
  /** When enabled, format the document before saving (requires a formatting-capable LSP server). */
  lspFormatOnSave: boolean;

  // Session Persistence
  /** Controls for the durable session persistence layer. */
  sessionPersistence: {
    /** Master toggle — when disabled, session state is not saved/restored on repo switch. */
    enabled: boolean;
    /** Keep terminal xterm instances alive (hidden) when switching repos. */
    terminalKeepAlive: boolean;
    /** Restore scroll positions when switching back to a repo. */
    scrollRestore: boolean;
    /** Maximum number of repos whose terminals are kept alive simultaneously (1–10). */
    terminalKeepAliveLimit: number;
  };
}

export const DEFAULT_SETTINGS: Settings = {
  theme: "dark",
  editorFontSize: 13,
  editorWordWrap: false,
  editorMinimap: true,
  showActivityBar: true,

  editorThemeDark: "github-dark",
  editorThemeLight: "github-light",

  maxPreviewKib: 512,
  parquetPreviewRows: 200,

  reopenReposOnStartup: true,
  defaultCloneDir: null,

  preferCrabDiff: true,

  discardConfirmThreshold: 40,

  conventionalCommits: false,
  stripNotebookOutputs: false,
  largeFileThresholdMb: 50,

  heapCapMb: 1024,

  maxLogLines: 2000,
  agentLogLevel: "info",
  openDevtoolsOnLaunch: false,

  notifications: {
    muteJobCompletion: false,
    mutePrUpdates: false,
    muteAgentErrors: false,
    doNotDisturb: false,
    doNotDisturbUntil: null,
  },

  welcomeSeen: false,

  aiProvider: "none",
  aiEndpoint: null,
  aiModel: null,

  features: {
    worktrees: true,
    sshRemoteEnabled: true,
  },

  aiAssistantEnabled: false,

  orchestratorEnabled: true,
  orchestratorDefaultAgent: "claude-code",
  orchestratorClaudeCodePath: "",
  orchestratorCodexPath: "",
  orchestratorCustomBinaryPath: "",
  orchestratorCustomBinaryName: "",
  orchestratorWorktreeBaseDir: "",
  orchestratorRetentionDays: 7,
  orchestratorMaxConcurrent: 3,
  orchestratorApiKeyPassthrough: false,
  orchestratorSetupScript: "",
  orchestratorRunScript: "",
  orchestratorArchiveScript: "",
  orchestratorNonconcurrentRun: false,
  orchestratorPortRangeStart: 3100,
  orchestratorPortRangeEnd: 3199,

  telemetryOptIn: false,
  telemetryPromptSeen: false,

  lspFormatOnSave: false,

  sessionPersistence: {
    enabled: true,
    terminalKeepAlive: true,
    scrollRestore: true,
    terminalKeepAliveLimit: 5,
  },
};

const LS_KEY = "crab.settings.v1";

function load(): Settings {
  try {
    const raw = localStorage.getItem(LS_KEY);
    if (!raw) return DEFAULT_SETTINGS;
    const parsed = JSON.parse(raw) as Partial<Settings>;
    // Spread default first, then overlay saved — missing keys get
    // defaults, stale keys are dropped.
    return { ...DEFAULT_SETTINGS, ...parsed };
  } catch {
    return DEFAULT_SETTINGS;
  }
}

/**
 * Read the current settings snapshot from localStorage.
 * Safe to call outside React (e.g. from toast publishers).
 */
export function getSettings(): Settings {
  return load();
}

function save(s: Settings) {
  try {
    localStorage.setItem(LS_KEY, JSON.stringify(s));
  } catch {
    // Quota exceeded or disabled — ignore. Settings still apply for
    // the current session.
  }
}

// ---- Context ----------------------------------------------------------

interface Ctx {
  settings: Settings;
  update: <K extends keyof Settings>(key: K, value: Settings[K]) => void;
  reset: () => void;
}

const SettingsContext = createContext<Ctx | null>(null);

export function SettingsProvider({ children }: { children: React.ReactNode }) {
  const [settings, setSettings] = useState<Settings>(() => load());

  // Persist on every change. Saving is cheap (a stringify + a
  // localStorage write) so we don't bother debouncing.
  useEffect(() => {
    save(settings);
  }, [settings]);

  // Apply the theme class on <html> reactively. The initial class
  // is set by the inline script in index.html to avoid a flash; this
  // effect keeps the class in sync when the user changes the setting
  // at runtime and when the system scheme switches (theme === "system").
  useEffect(() => {
    const root = document.documentElement;
    const apply = () => {
      root.classList.remove("theme-dark", "theme-light", "theme-hc-dark", "theme-hc-light", "dark");
      if (settings.theme === "system") {
        const mq = window.matchMedia("(prefers-color-scheme: light)");
        const isDark = !mq.matches;
        root.classList.add(isDark ? "theme-dark" : "theme-light");
        if (isDark) root.classList.add("dark");
      } else {
        root.classList.add(`theme-${settings.theme}`);
        const isDark = settings.theme === "dark" || settings.theme === "hc-dark";
        if (isDark) root.classList.add("dark");
      }
    };
    apply();
    if (settings.theme === "system") {
      const mq = window.matchMedia("(prefers-color-scheme: light)");
      const handler = () => apply();
      mq.addEventListener("change", handler);
      return () => mq.removeEventListener("change", handler);
    }
    return undefined;
  }, [settings.theme]);

  // Mirror the subset of settings main-process code needs. Today:
  //   - agentLogLevel → env var on the sidecar (next launch)
  //   - openDevtoolsOnLaunch → picked up at window-create time
  // Both write through to `~/Library/Application Support/
  // crab-desktop/main-settings.json`, which the Electron main
  // process reads at startup. The main process also applies the
  // DevTools toggle live.
  useEffect(() => {
    mainProcess
      .updateSettings({
        agentLogLevel: settings.agentLogLevel,
        openDevtoolsOnLaunch: settings.openDevtoolsOnLaunch,
      })
      .catch(() => undefined);
    mainProcess.setDevtools(settings.openDevtoolsOnLaunch).catch(() => undefined);
  }, [settings.agentLogLevel, settings.openDevtoolsOnLaunch]);

  const update = useCallback<Ctx["update"]>((key, value) => {
    setSettings((prev) => ({ ...prev, [key]: value }));
  }, []);

  const reset = useCallback(() => setSettings(DEFAULT_SETTINGS), []);

  const value = useMemo<Ctx>(() => ({ settings, update, reset }), [settings, update, reset]);
  return <SettingsContext.Provider value={value}>{children}</SettingsContext.Provider>;
}

export function useSettings(): Ctx {
  const ctx = useContext(SettingsContext);
  if (!ctx) throw new Error("useSettings used outside SettingsProvider");
  return ctx;
}

export function useSetting<K extends keyof Settings>(key: K): Settings[K] {
  return useSettings().settings[key];
}
