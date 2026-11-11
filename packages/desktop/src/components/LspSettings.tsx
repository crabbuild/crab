import { Input } from "@/components/ui/input";
import { Textarea } from "@/components/ui/textarea";
/**
 * LspSettings — "Language Servers" section for the Settings view.
 *
 * Lists discovered servers with enable/disable toggles, per-server config
 * editors, install guide links, format-on-save toggles, and diagnostic
 * display toggles.
 */

import { useCallback, useEffect, useState } from "react";
import {
  Check,
  ChevronDown,
  ChevronRight,
  ExternalLink,
  Loader2,
  Minus,
  X,
} from "lucide-react";
import type { LspDiscoveredServer, LspServerStatusInfo } from "../ipc";

// ── Install guide URLs ──────────────────────────────────────────────

const INSTALL_GUIDES: Record<string, { label: string; url: string }> = {
  typescript: {
    label: "typescript-language-server",
    url: "https://www.npmjs.com/package/typescript-language-server",
  },
  rust: {
    label: "rust-analyzer",
    url: "https://rust-analyzer.github.io",
  },
  python: {
    label: "python-lsp-server",
    url: "https://github.com/python-lsp/python-lsp-server",
  },
  go: {
    label: "gopls",
    url: "https://pkg.go.dev/golang.org/x/tools/gopls",
  },
};

// ── Known languages for display ─────────────────────────────────────

const KNOWN_LANGUAGES = ["typescript", "rust", "python", "go"];

// ── Types ───────────────────────────────────────────────────────────

export interface LspServerSettings {
  enabled: boolean;
  command: string;
  args: string[];
  initializationOptions: string; // JSON string
}

export interface LspSettingsConfig {
  servers: Record<string, LspServerSettings>;
  formatOnSave: Record<string, boolean>;
  diagnostics: {
    showInFileTree: boolean;
    showInTabs: boolean;
  };
}

interface Props {
  config: LspSettingsConfig;
  onConfigChange: (config: LspSettingsConfig) => void;
}

// ── Component ───────────────────────────────────────────────────────

export function LspSettings({ config, onConfigChange }: Props) {
  const [discovered, setDiscovered] = useState<LspDiscoveredServer[]>([]);
  const [statuses, setStatuses] = useState<LspServerStatusInfo[]>([]);
  const [expandedServer, setExpandedServer] = useState<string | null>(null);

  useEffect(() => {
    window.crab.lsp.discover().then(setDiscovered).catch(() => undefined);
    window.crab.lsp.status().then(setStatuses).catch(() => undefined);

    const unsubscribe = window.crab.lsp.onStatusChange((payload) => {
      setStatuses((prev) => {
        const idx = prev.findIndex((s) => s.language === payload.language);
        if (idx >= 0) {
          const updated = [...prev];
          updated[idx] = payload;
          return updated;
        }
        return [...prev, payload];
      });
    });

    return unsubscribe;
  }, []);

  const toggleServer = useCallback(
    (language: string) => {
      const current = config.servers[language];
      const updated: LspSettingsConfig = {
        ...config,
        servers: {
          ...config.servers,
          [language]: {
            ...(current ?? { command: "", args: [], initializationOptions: "{}" }),
            enabled: !(current?.enabled ?? true),
          },
        },
      };
      onConfigChange(updated);
    },
    [config, onConfigChange],
  );

  const updateServerConfig = useCallback(
    (language: string, patch: Partial<LspServerSettings>) => {
      const current = config.servers[language] ?? {
        enabled: true,
        command: "",
        args: [],
        initializationOptions: "{}",
      };
      const updated: LspSettingsConfig = {
        ...config,
        servers: {
          ...config.servers,
          [language]: { ...current, ...patch },
        },
      };
      onConfigChange(updated);
    },
    [config, onConfigChange],
  );

  const toggleFormatOnSave = useCallback(
    (language: string) => {
      const updated: LspSettingsConfig = {
        ...config,
        formatOnSave: {
          ...config.formatOnSave,
          [language]: !config.formatOnSave[language],
        },
      };
      onConfigChange(updated);
    },
    [config, onConfigChange],
  );

  const toggleDiagnosticDisplay = useCallback(
    (key: "showInFileTree" | "showInTabs") => {
      const updated: LspSettingsConfig = {
        ...config,
        diagnostics: {
          ...config.diagnostics,
          [key]: !config.diagnostics[key],
        },
      };
      onConfigChange(updated);
    },
    [config, onConfigChange],
  );

  // Merge discovered + known languages for display
  const allLanguages = Array.from(
    new Set([...KNOWN_LANGUAGES, ...discovered.map((d) => d.language)]),
  );

  return (
    <section className="space-y-4">
      <h3 className="text-sm font-medium text-foreground">Language Servers</h3>

      {/* Server list */}
      <div className="space-y-1">
        {allLanguages.map((language) => {
          const disc = discovered.find((d) => d.language === language);
          const status = statuses.find((s) => s.language === language);
          const serverConfig = config.servers[language];
          const enabled = serverConfig?.enabled ?? true;
          const isExpanded = expandedServer === language;

          return (
            <div key={language} className="rounded border border-border bg-background">
              {/* Server row */}
              <div className="flex items-center gap-2 px-3 py-2">
                <button
                  type="button"
                  className="shrink-0 text-muted-foreground hover:text-foreground"
                  onClick={() => setExpandedServer(isExpanded ? null : language)}
                  aria-label={isExpanded ? "Collapse" : "Expand"}
                >
                  {isExpanded ? <ChevronDown size={12} /> : <ChevronRight size={12} />}
                </button>

                <span className="text-xs font-medium text-foreground capitalize flex-1">
                  {language}
                </span>

                {/* Status chip */}
                <ServerStatusChip status={status} found={!!disc} />

                {/* Enable/disable toggle */}
                <button
                  type="button"
                  onClick={() => toggleServer(language)}
                  className={`relative inline-flex h-4 w-7 shrink-0 items-center rounded-full transition ${
                    enabled ? "bg-primary" : "bg-border"
                  }`}
                  role="switch"
                  aria-checked={enabled}
                  aria-label={`${enabled ? "Disable" : "Enable"} ${language} language server`}
                >
                  <span
                    className={`inline-block h-3 w-3 rounded-full bg-white transition-transform ${
                      enabled ? "translate-x-3.5" : "translate-x-0.5"
                    }`}
                  />
                </button>
              </div>

              {/* Expanded config editor */}
              {isExpanded && (
                <ServerConfigEditor
                  language={language}
                  discovered={disc}
                  serverConfig={serverConfig}
                  onUpdate={(patch) => updateServerConfig(language, patch)}
                />
              )}
            </div>
          );
        })}
      </div>

      {/* Format on Save */}
      <div className="space-y-2">
        <h4 className="text-xs font-medium text-muted-foreground uppercase tracking-wide">
          Format on Save
        </h4>
        <div className="space-y-1">
          {allLanguages.map((language) => (
            <label
              key={language}
              className="flex items-center gap-2 px-3 py-1 text-xs text-foreground cursor-pointer hover:bg-accent rounded"
            >
              <input
                type="checkbox"
                checked={config.formatOnSave[language] ?? false}
                onChange={() => toggleFormatOnSave(language)}
                className="rounded border-border text-primary focus:ring-primary h-3 w-3"
              />
              <span className="capitalize">{language}</span>
            </label>
          ))}
        </div>
      </div>

      {/* Diagnostic display toggles */}
      <div className="space-y-2">
        <h4 className="text-xs font-medium text-muted-foreground uppercase tracking-wide">
          Diagnostic Display
        </h4>
        <label className="flex items-center gap-2 px-3 py-1 text-xs text-foreground cursor-pointer hover:bg-accent rounded">
          <input
            type="checkbox"
            checked={config.diagnostics.showInFileTree}
            onChange={() => toggleDiagnosticDisplay("showInFileTree")}
            className="rounded border-border text-primary focus:ring-primary h-3 w-3"
          />
          Show diagnostics in file tree
        </label>
        <label className="flex items-center gap-2 px-3 py-1 text-xs text-foreground cursor-pointer hover:bg-accent rounded">
          <input
            type="checkbox"
            checked={config.diagnostics.showInTabs}
            onChange={() => toggleDiagnosticDisplay("showInTabs")}
            className="rounded border-border text-primary focus:ring-primary h-3 w-3"
          />
          Show diagnostics in editor tabs
        </label>
      </div>
    </section>
  );
}

// ── Sub-components ──────────────────────────────────────────────────

function ServerStatusChip({
  status,
  found,
}: {
  status: LspServerStatusInfo | undefined;
  found: boolean;
}) {
  if (!found && !status) {
    return (
      <span className="flex items-center gap-1 text-[10px] text-muted-foreground">
        <Minus size={9} />
        Not found
      </span>
    );
  }

  const state = status?.state ?? "discovered";

  switch (state) {
    case "running":
      return (
        <span className="flex items-center gap-1 text-[10px] text-success">
          <Check size={9} />
          Running
        </span>
      );
    case "restarting":
      return (
        <span className="flex items-center gap-1 text-[10px] text-warning">
          <Loader2 size={9} className="animate-spin" />
          Starting
        </span>
      );
    case "error":
      return (
        <span className="flex items-center gap-1 text-[10px] text-destructive">
          <X size={9} />
          Error
        </span>
      );
    case "stopped":
      return (
        <span className="flex items-center gap-1 text-[10px] text-muted-foreground">
          <Minus size={9} />
          Stopped
        </span>
      );
    default:
      return (
        <span className="flex items-center gap-1 text-[10px] text-muted-foreground">
          <Minus size={9} />
          Discovered
        </span>
      );
  }
}

function ServerConfigEditor({
  language,
  discovered,
  serverConfig,
  onUpdate,
}: {
  language: string;
  discovered: LspDiscoveredServer | undefined;
  serverConfig: LspServerSettings | undefined;
  onUpdate: (patch: Partial<LspServerSettings>) => void;
}) {
  const command = serverConfig?.command ?? discovered?.path ?? "";
  const args = serverConfig?.args ?? [];
  const initOpts = serverConfig?.initializationOptions ?? "{}";
  const guide = INSTALL_GUIDES[language];

  return (
    <div className="border-t border-border px-3 py-2 space-y-2">
      {/* Command */}
      <div>
        <label className="block text-[10px] text-muted-foreground mb-0.5">Command</label>
        <Input
          type="text"
          value={command}
          onChange={(e) => onUpdate({ command: e.target.value })}
          placeholder={discovered?.command ?? "path/to/server"}
          className="w-full rounded border border-border bg-card px-2 py-1 text-xs text-foreground placeholder:text-muted-foreground/50 focus:border-primary focus:outline-none"
        />
      </div>

      {/* Args */}
      <div>
        <label className="block text-[10px] text-muted-foreground mb-0.5">Arguments</label>
        <Input
          type="text"
          value={args.join(" ")}
          onChange={(e) =>
            onUpdate({ args: e.target.value.split(" ").filter(Boolean) })
          }
          placeholder="--stdio"
          className="w-full rounded border border-border bg-card px-2 py-1 text-xs text-foreground placeholder:text-muted-foreground/50 focus:border-primary focus:outline-none"
        />
      </div>

      {/* Initialization Options */}
      <div>
        <label className="block text-[10px] text-muted-foreground mb-0.5">
          Initialization Options (JSON)
        </label>
        <Textarea
          value={initOpts}
          onChange={(e) => onUpdate({ initializationOptions: e.target.value })}
          placeholder="{}"
          rows={3}
          className="w-full rounded border border-border bg-card px-2 py-1 text-xs text-foreground font-mono placeholder:text-muted-foreground/50 focus:border-primary focus:outline-none resize-y"
        />
      </div>

      {/* Install guide link (shown when server not found) */}
      {!discovered && guide && (
        <button
          type="button"
          onClick={() => {
            if (typeof window.crab?.fsOpenExternal === "function") {
              window.crab.fsOpenExternal(guide.url);
            } else {
              window.open(guide.url, "_blank");
            }
          }}
          className="flex items-center gap-1 text-[11px] text-primary hover:underline"
        >
          <ExternalLink size={10} />
          Install {guide.label}
        </button>
      )}

      {/* Version info */}
      {discovered?.version && (
        <p className="text-[10px] text-muted-foreground">
          Version: {discovered.version}
        </p>
      )}
    </div>
  );
}
