import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
/**
 * Agent Manager View — browse, install, configure, and create agents.
 *
 * Accessible from the Activity Bar or the "Add Agent…" shortcut in the
 * ChatInput agent dropdown. Shows installed agents at the top with
 * configure options, available (uninstalled) agents below with one-click
 * install, and a "Create Custom Agent" card at the bottom.
 *
 * Also includes orchestrator task settings (worktree, scripts, preferences)
 * in a collapsible section — previously a separate "Orchestrator" settings page.
 */

import clsx from "clsx";
import {
  Bot,
  Check,
  ChevronDown,
  ChevronRight,
  Copy,
  ExternalLink,
  Key,
  Loader2,
  Plus,
  RefreshCw,
  Settings,
  Terminal,
} from "lucide-react";
import { useCallback, useEffect, useState } from "react";
import {
  AgentIcon,
  agentAccentBg,
  agentAccentColor,
} from "../components/orchestrator/AgentIcon";
import { OrchestratorSettings } from "../components/orchestrator/OrchestratorSettings";

// ── Types ───────────────────────────────────────────────────────────

interface DetectedAgent {
  type: string;
  name: string;
  binary: string;
  path: string;
  version: string;
  available: boolean;
}

type ManagerPanel = "list" | "install" | "configure" | "create-custom";

interface Props {
  /** Called when the user wants to navigate back (e.g. to the chat view). */
  onNavigateToChat?: () => void;
}

// ── Agent Catalog ───────────────────────────────────────────────────

/** Preset agent catalog — comprehensive list of terminal-capable coding agents.
 *
 *  Ordered by practical reputation: vendor strength, developer mindshare,
 *  and relevance for multi-agent orchestration. Install instructions verified
 *  against official docs where available (May 2026).
 *
 *  Phase 1 (terminal-native big 5) and Phase 2 (power-user agents) get
 *  first-class install UX. Phase 3 agents are included for discoverability.
 *  Everything else works through the generic adapter (Create Custom Agent). */
const AGENT_CATALOG: Record<string, {
  name: string;
  vendor: string;
  description: string;
  command: string;
  url: string;
  steps: string[];
  /** Internal ordering hint — not shown in UI. */
  category: "popular" | "community";
}> = {
  // ── Phase 1: Terminal-native big 5 ────────────────────────────────

  "claude-code": {
    name: "Claude Code",
    vendor: "Anthropic",
    description: "Agentic coding tool with plan mode, extended thinking, and 1M-token context. Multi-file edits, git workflows, and MCP extensibility.",
    command: "curl -fsSL https://claude.ai/install.sh | bash",
    url: "https://docs.anthropic.com/en/docs/claude-code/",
    steps: [
      "Run: curl -fsSL https://claude.ai/install.sh | bash",
      "Or via npm: npm install -g @anthropic-ai/claude-code",
      "Authenticate with Claude Pro/Max/Team subscription or API key",
      "Run `claude --version` to verify",
    ],
    category: "popular",
  },
  codex: {
    name: "Codex CLI",
    vendor: "OpenAI",
    description: "Terminal coding agent with GPT-5 models, sandboxed execution, and approval modes. Built in Rust. Also available as GitHub Action and SDK.",
    command: "npm install -g @openai/codex",
    url: "https://developers.openai.com/codex/cli",
    steps: [
      "Run: npm install -g @openai/codex",
      "Authenticate via ChatGPT OAuth (browser) or set OPENAI_API_KEY",
      "Run `codex --version` to verify",
    ],
    category: "popular",
  },
  "gemini-cli": {
    name: "Gemini CLI",
    vendor: "Google",
    description: "Open-source agent with Gemini models, 1M-token context, MCP support, Google Search, and a generous free tier (60 req/min).",
    command: "npm install -g @google/gemini-cli",
    url: "https://github.com/google-gemini/gemini-cli",
    steps: [
      "Run: npm install -g @google/gemini-cli",
      "Or via Homebrew: brew install gemini-cli",
      "Authenticate with Google account (free tier) or set GEMINI_API_KEY",
      "Run `gemini --version` to verify",
    ],
    category: "popular",
  },
  "github-copilot": {
    name: "GitHub Copilot CLI",
    vendor: "GitHub / Microsoft",
    description: "AI-powered CLI that can plan tasks, execute workflows, edit files, run tests, and iterate. Massive enterprise distribution.",
    command: "gh extension install github/gh-copilot",
    url: "https://docs.github.com/en/copilot/using-github-copilot/using-github-copilot-in-the-command-line",
    steps: [
      "Install GitHub CLI first: brew install gh (or see cli.github.com)",
      "Run: gh extension install github/gh-copilot",
      "Authenticate: gh auth login",
      "Run `gh copilot --help` to verify",
    ],
    category: "popular",
  },
  opencode: {
    name: "OpenCode",
    vendor: "SST / Anomaly (open source)",
    description: "Open-source Claude Code alternative with TUI, headless mode, LSP support, multi-session, and login support for Copilot/ChatGPT accounts.",
    command: "curl -fsSL https://opencode.ai/install | bash",
    url: "https://opencode.ai/docs",
    steps: [
      "Run: curl -fsSL https://opencode.ai/install | bash",
      "Set ANTHROPIC_API_KEY or configure another provider",
      "Run `opencode --version` to verify",
    ],
    category: "popular",
  },

  // ── Phase 2: Open-source and power-user agents ────────────────────

  "cursor-cli": {
    name: "Cursor CLI",
    vendor: "Anysphere",
    description: "CLI companion to the Cursor IDE. Strong developer mindshare with agentic terminal capabilities.",
    command: "cursor --cli",
    url: "https://www.cursor.com",
    steps: [
      "Install Cursor IDE from cursor.com",
      "Cursor CLI is bundled with the IDE installation",
      "Run `cursor --help` from terminal to verify",
    ],
    category: "popular",
  },
  cline: {
    name: "Cline CLI",
    vendor: "Cline (open source)",
    description: "Open-source agent platform for terminal, VS Code, and JetBrains. Plan & Act modes, browser automation, MCP, and SDK.",
    command: "npm install -g cline",
    url: "https://cline.bot",
    steps: [
      "Run: npm install -g cline",
      "Configure API provider (Anthropic, OpenAI, OpenRouter, etc.)",
      "Run `cline --version` to verify",
    ],
    category: "popular",
  },
  aider: {
    name: "Aider",
    vendor: "Aider AI / Paul Gauthier (open source)",
    description: "AI pair programming in your terminal. Git-native workflows, explicit file edits, auto-commits. Works with Claude, GPT, Gemini, DeepSeek, and local models.",
    command: "python -m pip install aider-chat",
    url: "https://aider.chat/docs/install",
    steps: [
      "Run: python -m pip install aider-chat",
      "Or: pip install aider-install && aider-install (uses uv)",
      "Set ANTHROPIC_API_KEY, OPENAI_API_KEY, or other provider key",
      "Run `aider --version` to verify",
    ],
    category: "popular",
  },
  goose: {
    name: "Goose",
    vendor: "Agentic AI Foundation (Linux Foundation)",
    description: "Open-source general-purpose AI agent. Desktop, CLI, and API. Supports 30+ LLM providers. MCP-oriented. Built in Rust.",
    command: "curl -fsSL https://github.com/block/goose/raw/main/download_cli.sh | bash",
    url: "https://github.com/block/goose",
    steps: [
      "macOS/Linux: curl -fsSL https://github.com/block/goose/raw/main/download_cli.sh | bash",
      "Run `goose configure` to set up your LLM provider",
      "Supports OpenAI, Anthropic, Ollama, Databricks, and 30+ more",
      "Run `goose --version` to verify",
    ],
    category: "popular",
  },
  kiro: {
    name: "Kiro CLI",
    vendor: "AWS",
    description: "AI-powered agentic CLI with spec-driven development, custom agents, MCP, steering, headless mode, and CI/CD workflows.",
    command: "curl -fsSL https://cli.kiro.dev/install | bash",
    url: "https://kiro.dev/",
    steps: [
      "Run: curl -fsSL https://cli.kiro.dev/install | bash",
      "Authenticate with AWS Builder ID or IAM Identity Center",
      "Run `kiro --version` to verify",
    ],
    category: "popular",
  },
  "amp-code": {
    name: "Amp CLI",
    vendor: "Sourcegraph",
    description: "Multiplayer coding agent with multi-repo context, code review, and thread sharing. Strong code intelligence from Sourcegraph.",
    command: "npm install -g @sourcegraph/amp",
    url: "https://ampcode.com",
    steps: [
      "Run: npm install -g @sourcegraph/amp",
      "Authenticate with Sourcegraph account",
      "Run `amp --version` to verify",
    ],
    category: "popular",
  },
  pi: {
    name: "Pi",
    vendor: "Inflection AI",
    description: "Conversational AI assistant with strong reasoning, empathetic dialogue, and coding capabilities. Terminal-native with streaming support.",
    command: "npm install -g @inflection/pi",
    url: "https://pi.ai",
    steps: [
      "Run: npm install -g @inflection/pi",
      "Authenticate with Pi account",
      "Run `pi --version` to verify",
    ],
    category: "popular",
  },

  // ── Phase 3: Model/ecosystem expansion ────────────────────────────

  "qwen-code": {
    name: "Qwen Code",
    vendor: "Alibaba Cloud",
    description: "Open-source terminal agent optimized for Qwen models and large-codebase understanding. Streaming and approval modes.",
    command: "npm install -g qwen-code",
    url: "https://github.com/QwenLM/qwen-code",
    steps: [
      "Run: npm install -g qwen-code",
      "Set DASHSCOPE_API_KEY environment variable",
      "Run `qwen-code --version` to verify",
    ],
    category: "community",
  },
  "kimi-cli": {
    name: "Kimi Code CLI",
    vendor: "Moonshot AI",
    description: "Terminal agent that reads/edits code, executes shell commands, fetches web pages, and plans/adapts during execution. Long-context support.",
    command: "npm install -g kimi-cli",
    url: "https://github.com/nicepkg/kimi-cli",
    steps: [
      "Run: npm install -g kimi-cli",
      "Set MOONSHOT_API_KEY environment variable",
      "Run `kimi --version` to verify",
    ],
    category: "community",
  },
  crush: {
    name: "Crush",
    vendor: "Charmbracelet (open source)",
    description: "Go-based terminal coding assistant with LLM-provider flexibility, LSP support, MCP servers, and agent skills. Beautiful TUI.",
    command: "go install github.com/charmbracelet/crush@latest",
    url: "https://github.com/charmbracelet/crush",
    steps: [
      "Run: go install github.com/charmbracelet/crush@latest",
      "Or via Homebrew: brew install charmbracelet/tap/crush",
      "Configure your LLM provider",
      "Run `crush --version` to verify",
    ],
    category: "community",
  },
  codebuff: {
    name: "Codebuff",
    vendor: "Codebuff AI",
    description: "Terminal coding agent that edits codebases through natural language and coordinates specialized agents for precise changes.",
    command: "npm install -g codebuff",
    url: "https://codebuff.com",
    steps: [
      "Run: npm install -g codebuff",
      "Authenticate with Codebuff account",
      "Run `codebuff --version` to verify",
    ],
    category: "community",
  },
  "openhands-cli": {
    name: "OpenHands CLI",
    vendor: "All Hands AI (open source)",
    description: "Full software-agent platform. Runs in terminal, IDE, CI pipelines, browser, or cloud sandboxes. Executes real engineering work.",
    command: "pip install openhands-ai",
    url: "https://github.com/All-Hands-AI/OpenHands",
    steps: [
      "Run: pip install openhands-ai",
      "Or use Docker: docker pull ghcr.io/all-hands-ai/openhands",
      "Configure your LLM provider",
      "Run `openhands --help` to verify",
    ],
    category: "community",
  },
  "continue-cli": {
    name: "Continue CLI",
    vendor: "Continue Dev (open source)",
    description: "Source-controlled AI checks and quality gates. Headless development workflows and CI integration.",
    command: "npm install -g @continuedev/continue",
    url: "https://github.com/continuedev/continue",
    steps: [
      "Run: npm install -g @continuedev/continue",
      "Configure in .continue/ directory",
      "Run `continue --help` to verify",
    ],
    category: "community",
  },
};

// ── Component ───────────────────────────────────────────────────────

export function AgentManagerView({ onNavigateToChat }: Props) {
  const [agents, setAgents] = useState<DetectedAgent[]>([]);
  const [loading, setLoading] = useState(true);
  const [panel, setPanel] = useState<ManagerPanel>("list");
  const [selectedAgent, setSelectedAgent] = useState<string | null>(null);
  const [installing, setInstalling] = useState<string | null>(null);
  const [copied, setCopied] = useState(false);

  // Custom agent form state
  const [customName, setCustomName] = useState("");
  const [customBinary, setCustomBinary] = useState("");
  const [customArgs, setCustomArgs] = useState("");

  const detectAgents = useCallback(async (force?: boolean) => {
    setLoading(true);
    try {
      const result = await (window as any).crab?.orchestrator?.detectAgents({ force });
      if (result?.success && result.agents) {
        setAgents(result.agents);
      }
    } catch {
      // Detection failed — show empty state
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    detectAgents();
  }, [detectAgents]);

  const installedAgents = agents.filter((a) => a.available);
  // Filter out the "custom" meta-type — it's not a real installable agent.
  // Merge detected-but-uninstalled agents with the full preset catalog so
  // users always see all popular agents even if the backend registry doesn't
  // have manifests for them.
  const detectedUninstalled = agents.filter((a) => !a.available && a.type !== "custom");
  const detectedTypes = new Set(agents.map((a) => a.type));

  // Build the full available list: detected uninstalled + catalog entries not yet detected.
  // Ordered by catalog definition order (vendor reputation).
  const catalogOnlyAgents: DetectedAgent[] = Object.entries(AGENT_CATALOG)
    .filter(([type]) => !detectedTypes.has(type))
    .map(([type, info]) => ({
      type,
      name: info.name,
      binary: type,
      path: "",
      version: "",
      available: false,
    }));

  // Merge: catalog order takes precedence, detected-but-not-in-catalog appended at end
  const catalogOrder = Object.keys(AGENT_CATALOG);
  const allAvailable = [...detectedUninstalled, ...catalogOnlyAgents];
  const availableAgents = allAvailable.sort((a, b) => {
    const ai = catalogOrder.indexOf(a.type);
    const bi = catalogOrder.indexOf(b.type);
    if (ai === -1 && bi === -1) return 0;
    if (ai === -1) return 1;
    if (bi === -1) return -1;
    return ai - bi;
  });

  const handleCopy = useCallback((text: string) => {
    navigator.clipboard.writeText(text);
    setCopied(true);
    setTimeout(() => setCopied(false), 2000);
  }, []);

  const handleInstallClick = useCallback((agentType: string) => {
    setSelectedAgent(agentType);
    setPanel("install");
  }, []);

  const handleConfigureClick = useCallback((agentType: string) => {
    setSelectedAgent(agentType);
    setPanel("configure");
  }, []);

  const handleRunInstall = useCallback(async (command: string, agentType: string) => {
    setInstalling(agentType);
    try {
      // The shell opens Terminal and delivers this after session restore.
      const syntheticTaskId = `install-${agentType}-${Date.now()}`;
      window.dispatchEvent(
        new CustomEvent("crab:spawn-agent-terminal", {
          detail: {
            name: `Install ${agentType}`,
            command,
            agentMeta: {
              taskId: syntheticTaskId,
              agentType,
              worktreePath: "",
            },
          },
        }),
      );

      // Listen for any terminal exit and re-detect agents. We don't have
      // the exact terminal ID (it's created by TerminalView), so we
      // re-detect on the next exit event after a short debounce.
      const crab = (window as any).crab;
      const off = crab.onTermExit(() => {
        off();
        setTimeout(() => {
          detectAgents(true);
          setInstalling(null);
        }, 1000);
      });

      // Safety timeout — clear the spinner after 5 minutes even if we
      // never receive an exit event (e.g. long-running install).
      setTimeout(() => {
        off();
        detectAgents(true);
        setInstalling(null);
      }, 5 * 60 * 1000);
    } catch {
      setInstalling(null);
    }
  }, [detectAgents]);

  const handleSaveCustomAgent = useCallback(async () => {
    if (!customName.trim() || !customBinary.trim()) return;
    try {
      await (window as any).crab?.orchestrator?.saveCustomAgent?.({
        name: customName.trim(),
        binary: customBinary.trim(),
        args: customArgs.trim().split(/\s+/).filter(Boolean),
      });
      setCustomName("");
      setCustomBinary("");
      setCustomArgs("");
      setPanel("list");
      detectAgents(true);
    } catch {
      // Save failed
    }
  }, [customName, customBinary, customArgs, detectAgents]);

  // ── Loading state ─────────────────────────────────────────────────

  if (loading) {
    return (
      <div className="flex h-full flex-col items-center justify-center gap-4">
        <Loader2 size={28} className="animate-spin text-primary" />
        <p className="text-xs text-muted-foreground">Detecting agents…</p>
      </div>
    );
  }

  // ── Install panel ─────────────────────────────────────────────────

  if (panel === "install" && selectedAgent) {
    const info = AGENT_CATALOG[selectedAgent];
    if (!info) {
      setPanel("list");
      return null;
    }

    return (
      <div className="flex h-full flex-col">
        <PanelHeader
          title={`Install ${info.name}`}
          onBack={() => setPanel("list")}
        />
        <div className="flex-1 overflow-y-auto p-4">
          <div className="mx-auto max-w-lg space-y-4">
            {/* Agent card */}
            <div className="flex items-center gap-3 rounded-lg border border-border bg-card p-4">
              <div className={clsx("flex h-10 w-10 items-center justify-center rounded-lg", agentAccentBg(selectedAgent))}>
                <AgentIcon agentName={selectedAgent} size={20} className={agentAccentColor(selectedAgent)} />
              </div>
              <div>
                <h3 className="text-sm font-medium text-foreground">
                  {info.name}
                </h3>
                <p className="text-xs text-muted-foreground">
                  {info.description}
                </p>
              </div>
            </div>

            {/* Agent card */}
            <div className="rounded-lg border border-border bg-background p-4">
              <div className="mb-2 flex items-center gap-1.5 text-[10px] font-medium uppercase tracking-wider text-muted-foreground">
                <Terminal size={11} />
                Install command
              </div>
              <div className="flex items-center gap-2 rounded-md border border-border bg-card px-3 py-2">
                <code className="flex-1 font-mono text-xs text-foreground">
                  {info.command}
                </code>
                <Button
                  variant="ghost"
                  size="compact"
                  type="button"
                  onClick={() => handleCopy(info.command)}
                  className="shrink-0 rounded px-2 py-0.5 text-[10px] text-primary hover:bg-primary/10 transition"
                >
                  {copied ? <Check size={12} /> : <Copy size={12} />}
                </Button>
              </div>

              {/* One-click install button */}
              <Button
                variant="default"
                size="compact"
                type="button"
                onClick={() => handleRunInstall(info.command, selectedAgent)}
                disabled={installing === selectedAgent}
                className="mt-3 flex w-full items-center justify-center gap-2 rounded-md bg-primary px-4 py-2.5 text-xs font-medium text-primary-foreground hover:bg-primary-hover transition disabled:opacity-50"
              >
                {installing === selectedAgent ? (
                  <>
                    <Loader2 size={13} className="animate-spin" />
                    Installing…
                  </>
                ) : (
                  <>
                    <Terminal size={13} />
                    Install in Terminal
                  </>
                )}
              </Button>
            </div>

            {/* Steps */}
            <div className="rounded-lg border border-border bg-card p-4">
              <h4 className="mb-2 text-[10px] font-medium uppercase tracking-wider text-muted-foreground">
                Setup steps
              </h4>
              <ol className="space-y-2">
                {info.steps.map((step, i) => (
                  <li key={i} className="flex gap-2 text-xs text-foreground">
                    <span className="shrink-0 font-mono text-primary">
                      {i + 1}.
                    </span>
                    <span>{step}</span>
                  </li>
                ))}
              </ol>
            </div>

            {/* Steps */}
            <a
              href={info.url}
              onClick={(e) => {
                e.preventDefault();
                (window as any).crab?.shell?.openExternal(info.url);
              }}
              className="flex items-center gap-1.5 text-xs text-primary hover:underline"
            >
              <ExternalLink size={11} />
              View documentation
            </a>

            {/* Documentation link */}
            <Button
              variant="ghost"
              size="compact"
              type="button"
              onClick={() => detectAgents(true)}
              className="flex items-center gap-1.5 text-xs text-muted-foreground hover:text-foreground transition"
            >
              <RefreshCw size={11} />
              Re-detect agents
            </Button>
          </div>
        </div>
      </div>
    );
  }

  // ── Configure panel ───────────────────────────────────────────────

  if (panel === "configure" && selectedAgent) {
    const agent = agents.find((a) => a.type === selectedAgent);

    return (
      <div className="flex h-full flex-col">
        <PanelHeader
          title={`Configure ${agent?.name ?? selectedAgent}`}
          onBack={() => setPanel("list")}
        />
        <div className="flex-1 overflow-y-auto p-4">
          <div className="mx-auto max-w-lg space-y-4">
            <AgentConfigurePanel
              agentType={selectedAgent}
              agent={agent ?? null}
            />
          </div>
        </div>
      </div>
    );
  }

  // ── Create custom agent panel ─────────────────────────────────────

  if (panel === "create-custom") {
    return (
      <div className="flex h-full flex-col">
        <PanelHeader
          title="Add Custom Agent (Generic Adapter)"
          onBack={() => setPanel("list")}
        />
        <div className="flex-1 overflow-y-auto p-4">
          <div className="mx-auto max-w-lg space-y-4">
            <p className="text-xs text-muted-foreground">
              Connect any CLI agent via the generic adapter protocol. The
              adapter normalizes stdin/stdout, structured logs, and event
              streams into a unified event model — so agents like Devin,
              Windsurf, SWE-agent, Warp, Open Interpreter, Plandex, and others
              work without bespoke integration.
            </p>

            <div className="rounded-lg border border-border bg-card p-3">
              <p className="text-[10px] text-muted-foreground leading-relaxed">
                <strong className="text-foreground">How it works:</strong> The
                adapter spawns your agent binary, captures its output (text,
                JSON, or stream-json), translates events into the normalized
                protocol (SessionStarted → FileEdited → DiffProduced →
                CommitSuggested → SessionCompleted), and feeds them into the
                orchestrator UI.
              </p>
            </div>

            {/* Name */}
            <div>
              <label className="mb-1 block text-[10px] font-medium uppercase tracking-wider text-muted-foreground">
                Display Name
              </label>
              <Input
                type="text"
                placeholder="My Custom Agent"
                value={customName}
                onChange={(e) => setCustomName(e.target.value)}
                className="w-full rounded-md border border-border bg-background px-3 py-2 text-xs text-foreground placeholder:text-muted-foreground/50 focus:border-primary focus:outline-none"
              />
            </div>

            {/* Name */}
            <div>
              <label className="mb-1 block text-[10px] font-medium uppercase tracking-wider text-muted-foreground">
                Binary Path or Command
              </label>
              <Input
                type="text"
                placeholder="/usr/local/bin/my-agent or just my-agent"
                value={customBinary}
                onChange={(e) => setCustomBinary(e.target.value)}
                className="w-full rounded-md border border-border bg-background px-3 py-2 font-mono text-xs text-foreground placeholder:text-muted-foreground/50 focus:border-primary focus:outline-none"
              />
              <p className="mt-1 text-[10px] text-muted-foreground">
                Must be on your PATH or an absolute path to the binary.
              </p>
            </div>

            {/* Args */}
            <div>
              <label className="mb-1 block text-[10px] font-medium uppercase tracking-wider text-muted-foreground">
                Arguments (optional)
              </label>
              <Input
                type="text"
                placeholder="--message {prompt} --no-stream"
                value={customArgs}
                onChange={(e) => setCustomArgs(e.target.value)}
                className="w-full rounded-md border border-border bg-background px-3 py-2 font-mono text-xs text-foreground placeholder:text-muted-foreground/50 focus:border-primary focus:outline-none"
              />
              <p className="mt-1 text-[10px] text-muted-foreground">
                Use <code className="rounded bg-card px-1">{"{prompt}"}</code>{" "}
                for the user message and{" "}
                <code className="rounded bg-card px-1">{"{cwd}"}</code> for the
                working directory.
              </p>
            </div>

            {/* Save */}
            <Button
              variant="default"
              size="compact"
              type="button"
              onClick={handleSaveCustomAgent}
              disabled={!customName.trim() || !customBinary.trim()}
              className="flex w-full items-center justify-center gap-2 rounded-md bg-primary px-4 py-2.5 text-xs font-medium text-primary-foreground hover:bg-primary-hover transition disabled:opacity-40 disabled:cursor-not-allowed"
            >
              <Plus size={13} />
              Add Agent
            </Button>
          </div>
        </div>
      </div>
    );
  }

  // ── Main list panel ───────────────────────────────────────────────

  return (
    <div className="flex h-full flex-col">
      {/* Header */}
      <div className="flex items-center gap-2 border-b border-border px-4 py-3">
        <Bot size={16} className="text-primary" />
        <h1 className="flex-1 text-sm font-semibold text-foreground">
          Agent Manager
        </h1>
        <Button
          variant="ghost"
          size="compact"
          type="button"
          onClick={() => detectAgents(true)}
          className="flex items-center gap-1 rounded-md border border-border px-2 py-1 text-[10px] text-muted-foreground hover:text-foreground hover:bg-accent transition"
          title="Re-detect agents"
        >
          <RefreshCw size={11} />
          Refresh
        </Button>
      </div>

      <div className="flex-1 overflow-y-auto p-4">
        <div className="mx-auto max-w-2xl space-y-6">
          {/* Installed agents */}
          <section>
            <h2 className="mb-3 text-[10px] font-semibold uppercase tracking-wider text-muted-foreground">
              Installed ({installedAgents.length})
            </h2>
            {installedAgents.length === 0 ? (
              <div className="rounded-lg border border-dashed border-border bg-background p-6 text-center">
                <Bot size={24} className="mx-auto mb-2 text-muted-foreground/40" />
                <p className="text-xs text-muted-foreground">
                  No agents installed yet.
                </p>
                <p className="mt-1 text-[10px] text-muted-foreground">
                  Install one below to get started.
                </p>
              </div>
            ) : (
              <div className="grid gap-3 sm:grid-cols-2">
                {installedAgents.map((agent) => (
                  <AgentCard
                    key={agent.type}
                    agent={agent}
                    installed
                    onConfigure={() => handleConfigureClick(agent.type)}
                    onSelect={() => {
                      if (onNavigateToChat) onNavigateToChat();
                    }}
                  />
                ))}
              </div>
            )}
          </section>

          {/* Available agents — grouped by popularity */}
          {availableAgents.length > 0 &&
            (() => {
              const popular = availableAgents.filter((a) => AGENT_CATALOG[a.type]?.category === "popular");
              const community = availableAgents.filter((a) => AGENT_CATALOG[a.type]?.category !== "popular");

              return (
                <>
                  {popular.length > 0 && (
                    <section>
                      <h2 className="mb-3 text-[10px] font-semibold uppercase tracking-wider text-muted-foreground">
                        Popular ({popular.length})
                      </h2>
                      <div className="grid gap-3 sm:grid-cols-2">
                        {popular.map((agent) => (
                        <AgentCard
                          key={agent.type}
                          agent={agent}
                          installed={false}
                          installing={installing === agent.type}
                          onInstall={() => handleInstallClick(agent.type)}
                        />
                      ))}
                      </div>
                    </section>
                  )}

                  {community.length > 0 && (
                    <section>
                      <h2 className="mb-3 text-[10px] font-semibold uppercase tracking-wider text-muted-foreground">
                        Community ({community.length})
                      </h2>
                      <div className="grid gap-3 sm:grid-cols-2">
                        {community.map((agent) => (
                        <AgentCard
                          key={agent.type}
                          agent={agent}
                          installed={false}
                          installing={installing === agent.type}
                          onInstall={() => handleInstallClick(agent.type)}
                        />
                      ))}
                      </div>
                    </section>
                  )}
                </>
              );
            })()}

          {/* Create custom agent — generic adapter */}
          <section>
            <Button
              variant="ghost"
              size="compact"
              type="button"
              onClick={() => setPanel("create-custom")}
              className="justify-start flex w-full items-center gap-3 rounded-lg border border-dashed border-border bg-background p-4 text-left hover:border-primary/50 hover:bg-primary/5 transition group"
            >
              <div className="flex h-10 w-10 items-center justify-center rounded-lg bg-card group-hover:bg-primary/10 transition">
                <Plus size={18} className="text-muted-foreground group-hover:text-primary transition" />
              </div>
              <div>
                <h3 className="text-xs font-medium text-foreground">
                  Add Custom Agent (Generic Adapter)
                </h3>
                <p className="text-[10px] text-muted-foreground">
                  Connect any CLI agent via stdin/stdout. Works with Devin,
                  Windsurf, SWE-agent, Warp, and 30+ others.
                </p>
              </div>
              <ChevronRight size={14} className="ml-auto text-muted-foreground" />
            </Button>
          </section>

          {/* Task Settings — orchestrator configuration (worktree, scripts, etc.) */}
          <TaskSettingsSection />
        </div>
      </div>
    </div>
  );
}

// ── Sub-components ──────────────────────────────────────────────────

/**
 * Collapsible section that embeds the orchestrator task settings
 * (worktree, scripts, preferences, etc.) directly in the Agent Manager.
 * This replaces the old separate "Orchestrator" settings page.
 */
function TaskSettingsSection() {
  const [expanded, setExpanded] = useState(true);

  return (
    <section className="border-t border-border/60 pt-4">
      <Button
        variant="ghost"
        size="compact"
        type="button"
        onClick={() => setExpanded(!expanded)}
        className="justify-start flex w-full items-center gap-2 text-left"
      >
        <Settings size={13} className="text-muted-foreground" />
        <h2 className="flex-1 text-xs font-semibold uppercase tracking-wider text-muted-foreground">
          Task Settings
        </h2>
        <ChevronDown
          size={13}
          className={clsx(
            "text-muted-foreground transition-transform",
            !expanded && "-rotate-90",
          )}
        />
      </Button>
      <p className="mt-1 text-[10px] text-muted-foreground">
        Worktree, scripts, preferences, and orchestrator configuration.
      </p>
      {expanded && (
        <div className="mt-4">
          <OrchestratorSettings />
        </div>
      )}
    </section>
  );
}

/**
 * Full configure panel for a single agent. Combines environment variables,
 * binary path override, extra CLI args, capabilities display, and test
 * connection into one polished view — matching the Codex configure screenshot.
 */
function AgentConfigurePanel({
  agentType,
  agent,
}: {
  agentType: string;
  agent: DetectedAgent | null;
}) {
  const [config, setConfig] = useState<Record<string, unknown>>({});
  const [showAdvanced, setShowAdvanced] = useState(false);
  const [testing, setTesting] = useState(false);
  const [testResult, setTestResult] = useState<"idle" | "success" | "failure">("idle");
  const [testError, setTestError] = useState("");
  const [saved, setSaved] = useState(false);
  const [capabilities, setCapabilities] = useState<Record<string, boolean>>({});

  // Load existing config from the orchestrator settings store.
  useEffect(() => {
    (window as any).crab?.orchestrator
      ?.getAgentConfig(agentType)
      .then((result: any) => {
        if (result?.success && result.config) setConfig(result.config);
      })
      .catch(() => {});
  }, [agentType]);

  // Load capabilities from the registry.
  useEffect(() => {
    (window as any).crab?.orchestrator
      ?.listRegisteredAgents()
      .then((result: any) => {
        if (result?.success && result.agents) {
          const reg = result.agents.find((a: any) => a.type === agentType);
          if (reg?.capabilities) setCapabilities(reg.capabilities);
        }
      })
      .catch(() => {});
  }, [agentType]);

  const handleSave = useCallback(() => {
    (window as any).crab?.orchestrator?.setAgentConfig(agentType, config).catch(() => {});
    setSaved(true);
    setTimeout(() => setSaved(false), 2000);
  }, [agentType, config]);

  const handleTest = useCallback(async () => {
    setTesting(true);
    setTestResult("idle");
    try {
      const result = await (window as any).crab?.orchestrator?.testAgent(agentType);
      if (result?.success) {
        setTestResult("success");
      } else {
        setTestResult("failure");
        setTestError(result?.error ?? "Agent not found on PATH");
      }
    } catch (err) {
      setTestResult("failure");
      setTestError(err instanceof Error ? err.message : "Test failed");
    } finally {
      setTesting(false);
    }
  }, [agentType]);

  const envVars = getRequiredEnv(agentType);
  const currentEnv = (config.env as Record<string, string>) ?? {};

  return (
    <>
      {/* Agent info card */}
      <div className="flex items-center gap-3 rounded-lg border border-border bg-card p-4">
        <div className={clsx("flex h-10 w-10 items-center justify-center rounded-lg", agentAccentBg(agentType))}>
          <AgentIcon agentName={agentType} size={20} className={agentAccentColor(agentType)} />
        </div>
        <div className="flex-1 min-w-0">
          <h3 className="text-sm font-medium text-foreground">
            {agent?.name ?? agentType}
          </h3>
          {agent?.version && (
            <p className="text-[10px] text-muted-foreground">
              v{agent.version} — {agent.path}
            </p>
          )}
        </div>
        {agent?.available && (
          <span className="rounded-full bg-success/15 px-2 py-0.5 text-[10px] font-medium text-success">
            Installed
          </span>
        )}
      </div>

      {/* Environment Variables */}
      <div className="rounded-lg border border-border bg-card p-4">
        <h4 className="mb-3 flex items-center gap-1.5 text-[10px] font-medium uppercase tracking-wider text-muted-foreground">
          <Key size={11} />
          Environment Variables
        </h4>
        <div className="space-y-3">
          {envVars.length > 0 ? (
            envVars.map((env) => (
              <div key={env.name}>
                <label className="mb-1 block text-[10px] text-muted-foreground">
                  {env.label}
                </label>
                <Input
                  type={env.secret ? "password" : "text"}
                  placeholder={env.name}
                  value={currentEnv[env.name] ?? ""}
                  onChange={(e) => {
                    const newEnv = { ...currentEnv, [env.name]: e.target.value };
                    setConfig((prev) => ({ ...prev, env: newEnv }));
                  }}
                  className="w-full rounded-md border border-border bg-background px-3 py-1.5 font-mono text-xs text-foreground placeholder:text-muted-foreground/50 focus:border-primary focus:outline-none"
                />
              </div>
            ))
          ) : (
            <p className="text-xs text-muted-foreground">
              No environment variables required. This agent uses its own
              authentication.
            </p>
          )}
        </div>
      </div>

      {/* Advanced settings (collapsed by default) */}
      <div className="rounded-lg border border-border bg-card">
        <Button
          variant="ghost"
          size="compact"
          type="button"
          onClick={() => setShowAdvanced(!showAdvanced)}
          className="justify-start flex w-full items-center gap-2 px-4 py-3 text-left"
        >
          <ChevronDown
            size={12}
            className={clsx(
              "text-muted-foreground transition-transform",
              !showAdvanced && "-rotate-90",
            )}
          />
          <span className="text-[10px] font-medium uppercase tracking-wider text-muted-foreground">
            Advanced
          </span>
        </Button>
        {showAdvanced && (
          <div className="space-y-3 border-t border-border/60 px-4 pb-4 pt-3">
            {/* Binary path override */}
            <div>
              <label className="mb-1 block text-[10px] text-muted-foreground">
                Binary path override
              </label>
              <Input
                type="text"
                placeholder={`(auto-detect from PATH)`}
                value={(config.binaryPath as string) ?? ""}
                onChange={(e) => setConfig((prev) => ({ ...prev, binaryPath: e.target.value || undefined }))}
                className="w-full rounded-md border border-border bg-background px-3 py-1.5 font-mono text-xs text-foreground placeholder:text-muted-foreground/50 focus:border-primary focus:outline-none"
              />
              <p className="mt-1 text-[9px] text-muted-foreground">
                Leave empty to use the binary found on PATH.
              </p>
            </div>

            {/* Extra CLI arguments */}
            <div>
              <label className="mb-1 block text-[10px] text-muted-foreground">
                Extra CLI arguments
              </label>
              <Input
                type="text"
                placeholder="--flag1 --flag2"
                value={((config.extraArgs as string[]) ?? []).join(" ")}
                onChange={(e) => {
                  const args = e.target.value.trim() ? e.target.value.trim().split(/\s+/) : undefined;
                  setConfig((prev) => ({ ...prev, extraArgs: args }));
                }}
                className="w-full rounded-md border border-border bg-background px-3 py-1.5 font-mono text-xs text-foreground placeholder:text-muted-foreground/50 focus:border-primary focus:outline-none"
              />
              <p className="mt-1 text-[9px] text-muted-foreground">
                Additional flags passed to the agent CLI on every invocation.
              </p>
            </div>
          </div>
        )}
      </div>

      {/* Capabilities */}
      {Object.keys(capabilities).length > 0 && (
        <div className="rounded-lg border border-border bg-card p-4">
          <h4 className="mb-3 text-[10px] font-medium uppercase tracking-wider text-muted-foreground">
            Capabilities
          </h4>
          <div className="flex flex-wrap gap-2">
            {capabilities.supportsStreaming && (
              <CapBadge label="Streaming" supported />
            )}
            {capabilities.supportsToolUse && (
              <CapBadge label="Tool Use" supported />
            )}
            {capabilities.supportsIteration && (
              <CapBadge label="Iteration" supported />
            )}
            {capabilities.supportsPlanMode && (
              <CapBadge label="Plan Mode" supported />
            )}
            {capabilities.supportsApprovalModes && (
              <CapBadge label="Approval Modes" supported />
            )}
            {!capabilities.supportsStreaming && (
              <CapBadge label="Streaming" supported={false} />
            )}
            {!capabilities.supportsIteration && (
              <CapBadge label="Iteration" supported={false} />
            )}
            {!capabilities.supportsPlanMode && (
              <CapBadge label="Plan Mode" supported={false} />
            )}
          </div>
        </div>
      )}

      {/* Test + Save */}
      <div className="flex items-center gap-3">
        <Button
          variant="ghost"
          size="compact"
          type="button"
          onClick={handleTest}
          disabled={testing}
          className="flex items-center gap-1.5 rounded-md border border-border px-3 py-2 text-[11px] font-medium text-foreground hover:bg-accent transition disabled:opacity-50"
        >
          {testing ? (
            <Loader2 size={12} className="animate-spin" />
          ) : (
            <Terminal size={12} />
          )}
          Test Connection
        </Button>
        {testResult === "success" && (
          <span className="text-[10px] text-success">✓ Agent responding</span>
        )}
        {testResult === "failure" && (
          <span className="text-[10px] text-destructive">✗ {testError}</span>
        )}
      </div>

      <Button
        variant="default"
        size="compact"
        type="button"
        onClick={handleSave}
        className="flex w-full items-center justify-center gap-2 rounded-md bg-primary px-4 py-2.5 text-xs font-medium text-primary-foreground hover:bg-primary-hover transition"
      >
        <Check size={13} />
        {saved ? "Saved!" : "Save Configuration"}
      </Button>
    </>
  );
}

/** Small badge showing whether a capability is supported. */
function CapBadge({ label, supported }: { label: string; supported: boolean }) {
  return (
    <span
      className={clsx(
        "inline-flex items-center gap-1 rounded-full px-2 py-0.5 text-[9px] font-medium",
        supported
          ? "bg-success/10 text-success"
          : "bg-muted-foreground/10 text-muted-foreground",
      )}
    >
      <span className={clsx("h-1.5 w-1.5 rounded-full", supported ? "bg-success" : "bg-muted-foreground/40")} />
      {label}
    </span>
  );
}

function PanelHeader({ title, onBack }: { title: string; onBack: () => void }) {
  return (
    <div className="flex items-center gap-2 border-b border-border px-4 py-3">
      <Button
        variant="ghost"
        size="compact"
        type="button"
        onClick={onBack}
        className="flex items-center gap-1 rounded-md p-1 text-muted-foreground hover:text-foreground hover:bg-accent transition"
        aria-label="Back"
      >
        <ChevronRight size={14} className="rotate-180" />
      </Button>
      <h1 className="flex-1 text-sm font-semibold text-foreground">{title}</h1>
    </div>
  );
}

function AgentCard({
  agent,
  installed,
  installing,
  onInstall,
  onConfigure,
  onSelect,
}: {
  agent: DetectedAgent;
  installed: boolean;
  installing?: boolean;
  onInstall?: () => void;
  onConfigure?: () => void;
  onSelect?: () => void;
}) {
  const info = AGENT_CATALOG[agent.type];

  return (
    <div className="rounded-lg border border-border bg-card p-4 transition hover:border-primary/30">
      <div className="flex items-start gap-3">
        <div className={clsx("flex h-9 w-9 items-center justify-center rounded-lg", agentAccentBg(agent.type))}>
          <AgentIcon agentName={agent.type} size={18} className={agentAccentColor(agent.type)} />
        </div>
        <div className="flex-1 min-w-0">
          <h3 className="text-xs font-medium text-foreground">{agent.name}</h3>
          {installed && agent.version && (
            <p className="text-[10px] text-muted-foreground">
              v{agent.version}
            </p>
          )}
          {!installed && info && (
            <p className="text-[10px] text-muted-foreground line-clamp-2">
              {info.description}
            </p>
          )}
          {info?.vendor && (
            <p className="text-[9px] text-muted-foreground/70 mt-0.5">
              {info.vendor}
            </p>
          )}
        </div>
      </div>

      <div className="mt-3 flex items-center gap-2">
        {installed ? (
          <>
            <Button
              variant="ghost"
              size="compact"
              type="button"
              onClick={onConfigure}
              className="flex items-center gap-1 rounded-md border border-border px-2 py-1 text-[10px] text-muted-foreground hover:text-foreground hover:bg-accent transition"
            >
              <Settings size={10} />
              Configure
            </Button>
            {onSelect && (
              <Button
                variant="ghost"
                size="compact"
                type="button"
                onClick={onSelect}
                className="flex items-center gap-1 rounded-md bg-primary/10 px-2 py-1 text-[10px] text-primary hover:bg-primary/20 transition"
              >
                Use in Chat
                <ChevronRight size={10} />
              </Button>
            )}
          </>
        ) : (
          <Button
            variant="default"
            size="compact"
            type="button"
            onClick={onInstall}
            disabled={installing}
            className="flex items-center gap-1.5 rounded-md bg-primary px-3 py-1.5 text-[10px] font-medium text-primary-foreground hover:bg-primary-hover transition disabled:opacity-50"
          >
            {installing ? (
              <>
                <Loader2 size={11} className="animate-spin" />
                Installing…
              </>
            ) : (
              <>
                <Plus size={11} />
                Install
              </>
            )}
          </Button>
        )}
      </div>
    </div>
  );
}

// ── Helpers ─────────────────────────────────────────────────────────

function getRequiredEnv(agentType: string): { name: string; label: string; secret?: boolean }[] {
  const envMap: Record<string, { name: string; label: string; secret?: boolean }[]> = {
    "claude-code": [{ name: "ANTHROPIC_API_KEY", label: "Anthropic API Key (or use Claude subscription)", secret: true }],
    codex: [{ name: "OPENAI_API_KEY", label: "OpenAI API Key (or use ChatGPT OAuth)", secret: true }],
    "gemini-cli": [{ name: "GEMINI_API_KEY", label: "Gemini API Key (or use Google login — free tier)", secret: true }],
    "github-copilot": [],
    opencode: [{ name: "ANTHROPIC_API_KEY", label: "API Key for your chosen provider", secret: true }],
    "cursor-cli": [],
    cline: [{ name: "ANTHROPIC_API_KEY", label: "API Key for your chosen provider", secret: true }],
    aider: [{ name: "ANTHROPIC_API_KEY", label: "Anthropic API Key (or OPENAI_API_KEY, etc.)", secret: true }],
    goose: [],
    kiro: [],
    "amp-code": [],
    "qwen-code": [{ name: "DASHSCOPE_API_KEY", label: "DashScope API Key", secret: true }],
    "kimi-cli": [{ name: "MOONSHOT_API_KEY", label: "Moonshot API Key", secret: true }],
    crush: [],
    codebuff: [],
    "openhands-cli": [],
    "continue-cli": [],
  };
  return envMap[agentType] ?? [];
}
