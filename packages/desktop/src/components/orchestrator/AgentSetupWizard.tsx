/**
 * Agent Setup Wizard — detects installed code agents and guides
 * the user through installation if none are available.
 */

import { useCallback, useEffect, useState } from "react";
import {
  Bot,
  Check,
  ChevronRight,
  ExternalLink,
  Loader2,
  RefreshCw,
  Terminal,
  X,
} from "lucide-react";

// ── Types ───────────────────────────────────────────────────────────

interface DetectedAgent {
  /** Registry identifier (e.g. "claude-code", "codex"). */
  type: string;
  name: string;
  binary: string;
  path: string;
  version: string;
  available: boolean;
}

type SetupStep = "detecting" | "results" | "install-guide";

interface Props {
  /** Called when at least one agent is detected and ready. */
  onReady: (agents: DetectedAgent[]) => void;
  /** Called when user wants to skip setup and proceed anyway. */
  onSkip?: () => void;
}

// ── Install instructions per agent ──────────────────────────────────

const INSTALL_INSTRUCTIONS: Record<string, { name: string; steps: string[]; url: string; command?: string }> = {
  claude: {
    name: "Claude Code",
    steps: [
      "Install via npm: npm install -g @anthropic-ai/claude-code",
      "Or download from https://claude.ai/code",
      "Ensure the `claude` binary is on your PATH",
      "Run `claude --version` to verify installation",
    ],
    url: "https://docs.anthropic.com/en/docs/claude-code",
    command: "npm install -g @anthropic-ai/claude-code",
  },
  codex: {
    name: "Codex CLI",
    steps: [
      "Install via npm: npm install -g @openai/codex",
      "Ensure the `codex` binary is on your PATH",
      "Set your OPENAI_API_KEY environment variable",
      "Run `codex --version` to verify installation",
    ],
    url: "https://github.com/openai/codex",
    command: "npm install -g @openai/codex",
  },
};

// ── Component ───────────────────────────────────────────────────────

export function AgentSetupWizard({ onReady, onSkip }: Props) {
  const [step, setStep] = useState<SetupStep>("detecting");
  const [agents, setAgents] = useState<DetectedAgent[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [selectedGuide, setSelectedGuide] = useState<string | null>(null);
  const [copied, setCopied] = useState(false);

  const detectAgents = useCallback(async (options?: { force?: boolean }) => {
    setStep("detecting");
    setError(null);
    try {
      const result = await (window as any).crab?.orchestrator?.detectAgents({ force: options?.force });
      if (result?.success && result.agents) {
        setAgents(result.agents);
        const available = result.agents.filter((a: DetectedAgent) => a.available);
        if (available.length > 0) {
          onReady(result.agents);
        }
        setStep("results");
      } else {
        setError(result?.error ?? "Failed to detect agents");
        setStep("results");
      }
    } catch (err) {
      setError(err instanceof Error ? err.message : "Detection failed");
      setStep("results");
    }
  }, [onReady]);

  useEffect(() => {
    detectAgents();
  }, []);

  const availableAgents = agents.filter((a) => a.available);

  const handleCopyCommand = useCallback((command: string) => {
    navigator.clipboard.writeText(command);
    setCopied(true);
    setTimeout(() => setCopied(false), 2000);
  }, []);

  // ── Detecting state ─────────────────────────────────────────────────

  if (step === "detecting") {
    return (
      <div className="flex h-full flex-col items-center justify-center gap-4 p-8">
        <Loader2 size={32} className="animate-spin text-gx-accent" />
        <div className="text-center">
          <h2 className="text-sm font-semibold text-gx-text">Detecting AI Agents</h2>
          <p className="mt-1 text-xs text-gx-muted">
            Scanning your PATH for Claude Code and Codex…
          </p>
        </div>
      </div>
    );
  }

  // ── Install guide for a specific agent ──────────────────────────────

  if (step === "install-guide" && selectedGuide) {
    const guide = INSTALL_INSTRUCTIONS[selectedGuide];
    if (!guide) {
      setStep("results");
      return null;
    }

    return (
      <div className="flex h-full flex-col items-center justify-center gap-5 p-8">
        <div className="w-full max-w-md">
          {/* Header */}
          <div className="mb-4 flex items-center gap-2">
            <button
              type="button"
              onClick={() => setStep("results")}
              className="rounded p-1 text-gx-muted hover:text-gx-text hover:bg-gx-surface-hover transition"
              aria-label="Back"
            >
              <ChevronRight size={14} className="rotate-180" />
            </button>
            <Bot size={18} className="text-gx-accent" />
            <h2 className="text-sm font-semibold text-gx-text">Install {guide.name}</h2>
          </div>

          {/* Steps */}
          <div className="space-y-3 rounded-lg border border-gx-border bg-gx-surface p-4">
            <ol className="space-y-2.5">
              {guide.steps.map((s, i) => (
                <li key={i} className="flex gap-2 text-xs text-gx-text">
                  <span className="shrink-0 font-mono text-gx-accent">{i + 1}.</span>
                  <span>{s}</span>
                </li>
              ))}
            </ol>

            {/* Quick install command */}
            {guide.command && (
              <div className="mt-3 rounded-md border border-gx-border bg-gx-bg p-2.5">
                <div className="mb-1 flex items-center gap-1.5 text-[10px] text-gx-muted">
                  <Terminal size={10} />
                  Quick install
                </div>
                <div className="flex items-center gap-2">
                  <code className="flex-1 font-mono text-xs text-gx-text">{guide.command}</code>
                  <button
                    type="button"
                    onClick={() => handleCopyCommand(guide.command!)}
                    className="shrink-0 rounded px-2 py-0.5 text-[10px] text-gx-accent hover:bg-gx-accent/10 transition"
                  >
                    {copied ? "Copied!" : "Copy"}
                  </button>
                </div>
              </div>
            )}

            {/* Documentation link */}
            <a
              href={guide.url}
              onClick={(e) => {
                e.preventDefault();
                (window as any).crab?.openUrl(guide.url);
              }}
              className="mt-2 flex items-center gap-1.5 text-xs text-gx-accent hover:underline"
            >
              <ExternalLink size={11} />
              View documentation
            </a>
          </div>

          {/* Re-detect button */}
          <button
            type="button"
            onClick={() => detectAgents({ force: true })}
            className="mt-4 flex w-full items-center justify-center gap-2 rounded-md bg-gx-accent px-4 py-2 text-xs font-medium text-white hover:bg-gx-accent/80 transition"
          >
            <RefreshCw size={12} />
            Re-detect agents
          </button>
        </div>
      </div>
    );
  }

  // ── Results state ───────────────────────────────────────────────────

  return (
    <div className="flex h-full flex-col items-center justify-center gap-5 p-8">
      <Bot size={36} className="text-gx-muted/60" />

      <div className="text-center">
        <h2 className="text-base font-semibold text-gx-text">Agent Setup</h2>
        <p className="mt-1 max-w-[360px] text-xs text-gx-muted">
          {availableAgents.length > 0
            ? "The following AI agents are ready to use."
            : "No AI agents detected on your system. Install at least one to get started."}
        </p>
      </div>

      {error && (
        <div className="rounded-md border border-gx-error/30 bg-gx-error/10 px-3 py-2 text-xs text-gx-error">
          {error}
        </div>
      )}

      {/* Agent list */}
      <div className="w-full max-w-sm space-y-2">
        {agents.map((agent) => (
          <div
            key={agent.binary}
            className="flex items-center gap-3 rounded-lg border border-gx-border bg-gx-surface px-4 py-3"
          >
            <div
              className={`flex h-8 w-8 items-center justify-center rounded-full ${
                agent.available ? "bg-gx-success/15" : "bg-gx-bg"
              }`}
            >
              {agent.available ? (
                <Check size={14} className="text-gx-success" />
              ) : (
                <X size={14} className="text-gx-muted" />
              )}
            </div>
            <div className="flex-1">
              <div className="text-xs font-medium text-gx-text">{agent.name}</div>
              {agent.available ? (
                <div className="text-[10px] text-gx-muted">
                  v{agent.version} — {agent.path}
                </div>
              ) : (
                <div className="text-[10px] text-gx-error/80">Not installed</div>
              )}
            </div>
            {!agent.available && (
              <button
                type="button"
                onClick={() => {
                  setSelectedGuide(agent.binary);
                  setStep("install-guide");
                }}
                className="rounded px-2 py-1 text-[10px] text-gx-accent hover:bg-gx-accent/10 transition"
              >
                Install →
              </button>
            )}
          </div>
        ))}

        {/* Show entries for agents not in the detected list */}
        {!agents.some((a) => a.binary === "claude") && (
          <AgentPlaceholder
            name="Claude Code"
            binary="claude"
            onInstall={() => {
              setSelectedGuide("claude");
              setStep("install-guide");
            }}
          />
        )}
        {!agents.some((a) => a.binary === "codex") && (
          <AgentPlaceholder
            name="Codex"
            binary="codex"
            onInstall={() => {
              setSelectedGuide("codex");
              setStep("install-guide");
            }}
          />
        )}
      </div>

      {/* Actions */}
      <div className="flex items-center gap-3">
        <button
          type="button"
          onClick={() => detectAgents({ force: true })}
          className="flex items-center gap-1.5 rounded-md border border-gx-border px-3 py-2 text-xs text-gx-muted hover:text-gx-text transition"
        >
          <RefreshCw size={12} />
          Re-detect
        </button>
        {availableAgents.length > 0 && (
          <button
            type="button"
            onClick={() => onReady(agents)}
            className="flex items-center gap-1.5 rounded-md bg-gx-accent px-4 py-2 text-xs font-medium text-white hover:bg-gx-accent/80 transition"
          >
            Continue
            <ChevronRight size={12} />
          </button>
        )}
        {onSkip && availableAgents.length === 0 && (
          <button
            type="button"
            onClick={onSkip}
            className="text-xs text-gx-muted hover:text-gx-text transition"
          >
            Skip for now
          </button>
        )}
      </div>
    </div>
  );
}

// ── Placeholder for agents not in the detected list ─────────────────

function AgentPlaceholder({
  name,
  binary: _binary,
  onInstall,
}: {
  name: string;
  binary: string;
  onInstall: () => void;
}) {
  return (
    <div className="flex items-center gap-3 rounded-lg border border-dashed border-gx-border bg-gx-bg px-4 py-3">
      <div className="flex h-8 w-8 items-center justify-center rounded-full bg-gx-surface">
        <X size={14} className="text-gx-muted" />
      </div>
      <div className="flex-1">
        <div className="text-xs font-medium text-gx-text">{name}</div>
        <div className="text-[10px] text-gx-error/80">Not installed</div>
      </div>
      <button
        type="button"
        onClick={onInstall}
        className="rounded px-2 py-1 text-[10px] text-gx-accent hover:bg-gx-accent/10 transition"
      >
        Install →
      </button>
    </div>
  );
}
