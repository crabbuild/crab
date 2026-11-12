/**
 * Terminal preset manager for agent CLI startup commands.
 *
 * Manages built-in presets (Claude Code, OpenAI Codex, Aider, Gemini CLI,
 * Amazon Q Developer CLI, Kiro CLI, OpenCode) and user-defined custom commands.
 * Each preset can have sub-options representing popular flags/modes.
 * Persists configuration to localStorage.
 */

// --- Types ---

export interface SubOption {
  id: string;
  label: string;
  /** The full command to run (replaces the parent command). */
  command: string;
  /** Short description shown as tooltip or secondary text. */
  description?: string;
  /**
   * When true, the command ends with a placeholder that requires user input.
   * The UI prompts for a value before executing.
   * Trailing space in `command` is the convention for "needs input".
   */
  needsInput?: boolean;
  /** Placeholder text for the input field (e.g., "model name"). */
  inputPlaceholder?: string;
  /**
   * Known choices for the input value. When provided, the UI shows
   * a nested sub-menu of these choices instead of a free-text input.
   */
  choices?: { id: string; label: string; value: string; description?: string }[];
}

export interface StartupCommand {
  id: string;
  name: string;
  command: string;
  category: "builtin" | "custom";
  /** Optional working directory override. */
  cwd?: string;
  /** Optional extra environment variables. */
  env?: Record<string, string>;
  /** Display order within category. */
  order: number;
  /** Whether this preset is visible in the dropdown. */
  visible: boolean;
  /** Icon identifier for the dropdown. */
  icon?: string;
  /** Popular sub-options (flags/modes) for this agent. */
  subOptions?: SubOption[];
  /** Agent metadata for daemon session tracking. Injected at spawn time. */
  agentMeta?: { taskId: string; agentType: string; worktreePath: string };
}

export interface PresetManager {
  getAll(): StartupCommand[];
  getGrouped(): { builtin: StartupCommand[]; custom: StartupCommand[] };
  addCustom(cmd: Omit<StartupCommand, "id" | "category" | "order">): StartupCommand;
  updateCustom(id: string, updates: Partial<StartupCommand>): void;
  removeCustom(id: string): void;
  reorder(ids: string[]): void;
  setVisibility(id: string, visible: boolean): void;
  validate(cmd: Pick<StartupCommand, "name" | "command">): { valid: boolean; errors: string[] };
}

// --- Storage ---

export const LS_PRESETS_KEY = "crab.termPresets";

interface PresetsStorage {
  builtinVisibility: Record<string, boolean>;
  builtinOrder: string[];
  customCommands: StartupCommand[];
}

// --- Built-in Presets ---

const BUILTIN_PRESETS: StartupCommand[] = [
  {
    id: "builtin-claude",
    name: "Claude Code",
    command: "claude",
    category: "builtin",
    order: 0,
    visible: true,
    icon: "claude",
    subOptions: [
      { id: "claude-resume", label: "Resume last session", command: "claude --resume", description: "Continue previous conversation" },
      { id: "claude-print", label: "Non-interactive", command: "claude -p", description: "Print mode — single response, no REPL" },
      { id: "claude-dangerously-skip", label: "Full auto (skip permissions)", command: "claude --dangerously-skip-permissions", description: "Skip all permission prompts" },
      { id: "claude-verbose", label: "Verbose", command: "claude --verbose", description: "Show detailed debug output" },
      {
        id: "claude-model", label: "Choose model", command: "claude --model ", description: "Specify model",
        needsInput: true, inputPlaceholder: "model name",
        choices: [
          { id: "claude-opus-4.7", label: "Claude Opus 4.7", value: "opus", description: "Newest flagship — best reasoning & agentic coding" },
          { id: "claude-sonnet-4.6", label: "Claude Sonnet 4.6", value: "sonnet", description: "Best price-to-performance for daily work" },
          { id: "claude-haiku-4.5", label: "Claude Haiku 4.5", value: "haiku", description: "Fastest, low-latency, cost-efficient" },
        ],
      },
    ],
  },
  {
    id: "builtin-codex",
    name: "OpenAI Codex",
    command: "codex",
    category: "builtin",
    order: 1,
    visible: true,
    icon: "codex",
    subOptions: [
      { id: "codex-full-auto", label: "Full auto", command: "codex --full-auto", description: "Autonomous mode — no confirmations" },
      { id: "codex-exec", label: "Non-interactive (exec)", command: "codex exec", description: "Run once and exit" },
      { id: "codex-search", label: "With web search", command: "codex --search", description: "Enable live web search" },
      {
        id: "codex-image", label: "With image", command: "codex -i ", description: "Attach an image",
        needsInput: true, inputPlaceholder: "path/to/image",
      },
      {
        id: "codex-model", label: "Choose model", command: "codex -m ", description: "Specify model",
        needsInput: true, inputPlaceholder: "model name",
        choices: [
          { id: "codex-gpt53-codex", label: "GPT-5.3-Codex", value: "gpt-5.3-codex", description: "Latest — best agentic coding" },
          { id: "codex-gpt53-spark", label: "GPT-5.3-Codex-Spark", value: "gpt-5.3-codex-spark", description: "Real-time edits, fast iteration" },
          { id: "codex-gpt52-codex", label: "GPT-5.2-Codex", value: "gpt-5.2-codex", description: "Strong long-horizon coding" },
          { id: "codex-gpt51-max", label: "GPT-5.1-Codex-Max", value: "gpt-5.1-codex-max", description: "Token-efficient reasoning" },
          { id: "codex-gpt55", label: "GPT-5.5", value: "gpt-5.5", description: "Newest general flagship" },
        ],
      },
    ],
  },
  {
    id: "builtin-aider",
    name: "Aider",
    command: "aider",
    category: "builtin",
    order: 2,
    visible: true,
    icon: "aider",
    subOptions: [
      { id: "aider-architect", label: "Architect mode", command: "aider --architect", description: "Use architect + editor model pair" },
      { id: "aider-watch", label: "Watch mode", command: "aider --watch", description: "Watch files and auto-apply changes" },
      { id: "aider-no-auto-commits", label: "No auto-commits", command: "aider --no-auto-commits", description: "Disable automatic git commits" },
      {
        id: "aider-sonnet", label: "Choose model", command: "aider --model ", description: "Specify model",
        needsInput: true, inputPlaceholder: "model name",
        choices: [
          { id: "aider-claude-opus", label: "Claude Opus 4.7", value: "claude/claude-opus-4-7", description: "Anthropic flagship — best reasoning" },
          { id: "aider-claude-sonnet", label: "Claude Sonnet 4.6", value: "claude/claude-sonnet-4-6", description: "Anthropic workhorse" },
          { id: "aider-gpt53-codex", label: "GPT-5.3-Codex", value: "openai/gpt-5.3-codex", description: "OpenAI latest coding model" },
          { id: "aider-deepseek", label: "DeepSeek R1", value: "deepseek/deepseek-r1", description: "DeepSeek reasoning model" },
          { id: "aider-gemini", label: "Gemini 3 Pro", value: "gemini/gemini-3-pro-preview", description: "Google flagship" },
        ],
      },
    ],
  },
  {
    id: "builtin-gemini",
    name: "Gemini CLI",
    command: "gemini",
    category: "builtin",
    order: 3,
    visible: true,
    icon: "gemini",
    subOptions: [
      { id: "gemini-yolo", label: "YOLO mode (auto-approve)", command: "gemini -y", description: "Auto-approve all tool calls" },
      { id: "gemini-sandbox", label: "Sandbox mode", command: "gemini --sandbox", description: "Run in sandboxed environment" },
      { id: "gemini-resume", label: "Resume session", command: "gemini --resume", description: "Resume previous conversation" },
      {
        id: "gemini-non-interactive", label: "Non-interactive", command: "gemini -p ", description: "Single prompt response",
        needsInput: true, inputPlaceholder: "your prompt",
      },
      {
        id: "gemini-model", label: "Choose model", command: "gemini --model ", description: "Specify model",
        needsInput: true, inputPlaceholder: "model name",
        choices: [
          { id: "gemini-3-pro", label: "Gemini 3 Pro", value: "gemini-3-pro-preview", description: "Latest flagship — complex reasoning" },
          { id: "gemini-3-flash", label: "Gemini 3 Flash", value: "gemini-3-flash-preview", description: "Fast, frontier-class performance" },
          { id: "gemini-2.5-pro", label: "Gemini 2.5 Pro", value: "gemini-2.5-pro", description: "Previous gen — 1M context" },
          { id: "gemini-2.5-flash", label: "Gemini 2.5 Flash", value: "gemini-2.5-flash", description: "Previous gen — fast & efficient" },
        ],
      },
    ],
  },
  {
    id: "builtin-q",
    name: "Amazon Q Developer CLI",
    command: "q chat",
    category: "builtin",
    order: 4,
    visible: true,
    icon: "q",
    subOptions: [
      { id: "q-trust-all", label: "Trust all tools", command: "q chat --trust-all-tools", description: "Allow all tools without confirmation" },
      { id: "q-resume", label: "Resume session", command: "q chat --resume", description: "Resume previous conversation" },
      {
        id: "q-agent", label: "Custom agent", command: "q chat --agent ", description: "Use a specific agent",
        needsInput: true, inputPlaceholder: "agent name",
      },
      { id: "q-no-interactive", label: "Non-interactive", command: "q chat --no-interactive", description: "Print response and exit" },
    ],
  },
  {
    id: "builtin-kiro",
    name: "Kiro CLI",
    command: "kiro-cli",
    category: "builtin",
    order: 5,
    visible: true,
    icon: "kiro",
    subOptions: [
      { id: "kiro-trust-all", label: "Trust all tools", command: "kiro-cli chat --trust-all-tools", description: "Allow all tools without confirmation" },
      { id: "kiro-resume", label: "Resume session", command: "kiro-cli chat --resume", description: "Resume previous conversation" },
      { id: "kiro-resume-picker", label: "Session picker", command: "kiro-cli chat --resume-picker", description: "Choose which session to resume" },
      {
        id: "kiro-agent", label: "Custom agent", command: "kiro-cli chat --agent ", description: "Use a specific agent",
        needsInput: true, inputPlaceholder: "agent name",
      },
      {
        id: "kiro-translate", label: "Translate (NL → shell)", command: "kiro-cli translate ", description: "Convert natural language to shell command",
        needsInput: true, inputPlaceholder: "describe what you want",
      },
    ],
  },
  {
    id: "builtin-opencode",
    name: "OpenCode",
    command: "opencode",
    category: "builtin",
    order: 6,
    visible: true,
    icon: "opencode",
    subOptions: [
      { id: "opencode-non-interactive", label: "Non-interactive", command: "opencode -n", description: "Run without TUI" },
      { id: "opencode-quiet", label: "Quiet mode", command: "opencode -q", description: "Minimal output" },
      {
        id: "opencode-provider", label: "Choose provider", command: "opencode --provider ", description: "Specify provider",
        needsInput: true, inputPlaceholder: "provider name",
        choices: [
          { id: "opencode-anthropic", label: "Anthropic", value: "anthropic", description: "Claude models" },
          { id: "opencode-openai", label: "OpenAI", value: "openai", description: "GPT models" },
          { id: "opencode-google", label: "Google", value: "google", description: "Gemini models" },
          { id: "opencode-aws", label: "AWS Bedrock", value: "aws", description: "AWS Bedrock models" },
        ],
      },
      {
        id: "opencode-model", label: "Choose model", command: "opencode --model ", description: "Specify model",
        needsInput: true, inputPlaceholder: "model name",
      },
    ],
  },
];

// --- Helpers ---

function generateId(): string {
  return `custom-${Date.now()}-${Math.random().toString(36).slice(2, 8)}`;
}

function loadStorage(): PresetsStorage {
  try {
    const raw = localStorage.getItem(LS_PRESETS_KEY);
    if (raw) {
      const parsed = JSON.parse(raw);
      return {
        builtinVisibility: parsed.builtinVisibility ?? {},
        builtinOrder: parsed.builtinOrder ?? BUILTIN_PRESETS.map((p) => p.id),
        customCommands: Array.isArray(parsed.customCommands) ? parsed.customCommands : [],
      };
    }
  } catch {
    // Fall through to defaults
  }
  return {
    builtinVisibility: {},
    builtinOrder: BUILTIN_PRESETS.map((p) => p.id),
    customCommands: [],
  };
}

function saveStorage(storage: PresetsStorage): void {
  localStorage.setItem(LS_PRESETS_KEY, JSON.stringify(storage));
}

// --- PresetManager Implementation ---

export function createPresetManager(): PresetManager {
  let storage = loadStorage();

  function getBuiltins(): StartupCommand[] {
    const orderMap = new Map(storage.builtinOrder.map((id, idx) => [id, idx]));
    return BUILTIN_PRESETS.map((preset) => ({
      ...preset,
      order: orderMap.get(preset.id) ?? preset.order,
      visible: storage.builtinVisibility[preset.id] ?? preset.visible,
    })).sort((a, b) => a.order - b.order);
  }

  function getCustoms(): StartupCommand[] {
    return [...storage.customCommands].sort((a, b) => a.order - b.order);
  }

  return {
    getAll(): StartupCommand[] {
      return [...getBuiltins(), ...getCustoms()];
    },

    getGrouped(): { builtin: StartupCommand[]; custom: StartupCommand[] } {
      return {
        builtin: getBuiltins().filter((p) => p.visible),
        custom: getCustoms().filter((p) => p.visible),
      };
    },

    addCustom(cmd: Omit<StartupCommand, "id" | "category" | "order">): StartupCommand {
      const maxOrder = storage.customCommands.reduce(
        (max, c) => Math.max(max, c.order),
        -1,
      );
      const newCmd: StartupCommand = {
        ...cmd,
        id: generateId(),
        category: "custom",
        order: maxOrder + 1,
      };
      storage.customCommands.push(newCmd);
      saveStorage(storage);
      return newCmd;
    },

    updateCustom(id: string, updates: Partial<StartupCommand>): void {
      const idx = storage.customCommands.findIndex((c) => c.id === id);
      if (idx === -1) return;
      storage.customCommands[idx] = {
        ...storage.customCommands[idx],
        ...updates,
        id, // prevent id override
        category: "custom", // prevent category override
      };
      saveStorage(storage);
    },

    removeCustom(id: string): void {
      storage.customCommands = storage.customCommands.filter((c) => c.id !== id);
      saveStorage(storage);
    },

    reorder(ids: string[]): void {
      // Separate builtin and custom IDs
      const builtinIds = ids.filter((id) => id.startsWith("builtin-"));
      const customIds = ids.filter((id) => !id.startsWith("builtin-"));

      if (builtinIds.length > 0) {
        storage.builtinOrder = builtinIds;
      }

      if (customIds.length > 0) {
        // Reorder custom commands based on the provided ID order
        const cmdMap = new Map(storage.customCommands.map((c) => [c.id, c]));
        const reordered: StartupCommand[] = [];
        for (let i = 0; i < customIds.length; i++) {
          const cmd = cmdMap.get(customIds[i]);
          if (cmd) {
            reordered.push({ ...cmd, order: i });
          }
        }
        // Keep any commands not in the reorder list at the end
        for (const cmd of storage.customCommands) {
          if (!customIds.includes(cmd.id)) {
            reordered.push({ ...cmd, order: reordered.length });
          }
        }
        storage.customCommands = reordered;
      }

      saveStorage(storage);
    },

    setVisibility(id: string, visible: boolean): void {
      if (id.startsWith("builtin-")) {
        storage.builtinVisibility[id] = visible;
      } else {
        const idx = storage.customCommands.findIndex((c) => c.id === id);
        if (idx !== -1) {
          storage.customCommands[idx].visible = visible;
        }
      }
      saveStorage(storage);
    },

    validate(cmd: Pick<StartupCommand, "name" | "command">): { valid: boolean; errors: string[] } {
      const errors: string[] = [];
      if (!cmd.name || cmd.name.trim().length === 0) {
        errors.push("Name is required");
      }
      if (!cmd.command || cmd.command.trim().length === 0) {
        errors.push("Command is required");
      }
      return { valid: errors.length === 0, errors };
    },
  };
}

/** Default export for convenience. */
export const BUILTIN_PRESET_LIST = BUILTIN_PRESETS;
