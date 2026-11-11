import { useEffect, useState } from "react";
import type { LucideIcon } from "lucide-react";
import { publishToast } from "./toasts";
import { recordOperation } from "./operation-history";
import { isCancelledError, isStaleHunkError } from "../ipc";
import type { SelectionTransfers } from "../hooks/useFileTransfers";

// --- Types & Interfaces ---

export type CommandCategory =
  | "Commit"
  | "Sync"
  | "Branch"
  | "History"
  | "Stash"
  | "Stage"
  | "Worktree"
  | "Conflict"
  | "LargeFile"
  | "PullRequest"
  | "Browser";

export interface CommandContext {
  repoId: string;
  /** Filesystem path to the repo working directory. May be undefined for remote repos. */
  repoPath?: string;
  fileTransfers?: SelectionTransfers;
  currentBranch: string;
  upstream: string | null;
  ahead: number;
  behind: number;
  dirtyCount: number;
  hasStagedChanges: boolean;
  activeOperation: "merge" | "rebase" | "cherry-pick" | null;
  forgeProvider: "github" | "gitlab" | "bitbucket" | "gitea" | null;
}

export interface CommandResult {
  success: boolean;
  message: string;
  cancelled?: boolean;
}

export type PromptKind = "branch" | "commit" | "ref" | "choice";

export interface PromptConfig {
  kind: PromptKind;
  placeholder: string;
  choices?: Array<{ label: string; value: string }>;
  next?: PromptConfig;
}

export interface Command {
  id: string;
  label: string;
  category: CommandCategory;
  icon?: LucideIcon;
  shortcut?: string;
  prompt?: PromptConfig;
  /** Whether this command is destructive; its handler owns confirmation. */
  destructive?: boolean;
  isEnabled: (ctx: CommandContext) => boolean;
  execute: (ctx: CommandContext, input?: string) => Promise<CommandResult>;
}

// --- Pub-sub internals ---

type RegistrySubscriber = (commands: Command[]) => void;

let commands: Command[] = [];
const subscribers = new Set<RegistrySubscriber>();

function notify() {
  const snapshot = [...commands];
  for (const sub of subscribers) {
    sub(snapshot);
  }
}



// Fuzzy filter — imported lazily to avoid circular deps during early bootstrap.
// Falls back to simple substring match if the module isn't available yet.
let fuzzyFilterFn: ((cmds: Command[], query: string) => Array<{ command: Command; score: number }>) | null = null;

async function ensureFuzzyFilter() {
  if (fuzzyFilterFn) return;
  try {
    const mod = await import(/* @vite-ignore */ "./command-filter");
    fuzzyFilterFn = mod.fuzzyFilterCommands;
  } catch {
    // Module not yet available
  }
}

// Eagerly attempt to load the fuzzy filter on module init
void ensureFuzzyFilter();

// --- Public API ---

/**
 * Register a command in the palette registry.
 * If a command with the same id already exists, it is replaced.
 */
export function registerCommand(cmd: Command): void {
  commands = commands.filter((c) => c.id !== cmd.id);
  commands = [...commands, cmd];
  notify();
}

/**
 * Remove a command from the registry by id.
 */
export function unregisterCommand(id: string): void {
  commands = commands.filter((c) => c.id !== id);
  notify();
}

/**
 * Returns all registered commands.
 */
export function getCommands(): Command[] {
  return [...commands];
}

/**
 * Look up a single command by id.
 */
export function getCommand(id: string): Command | undefined {
  return commands.find((c) => c.id === id);
}

/**
 * Filter commands by fuzzy query, excluding disabled commands from results.
 * Delegates to the fuzzy filter module for scoring and ranking.
 */
export function filterCommands(query: string, ctx: CommandContext): Command[] {
  const enabled = commands.filter((c) => c.isEnabled(ctx));
  if (!query) return enabled;

  if (fuzzyFilterFn) {
    return fuzzyFilterFn(enabled, query).map((r) => r.command);
  }

  // Fallback: simple case-insensitive substring match
  const lower = query.toLowerCase();
  return enabled.filter(
    (c) => c.label.toLowerCase().includes(lower) || c.category.toLowerCase().includes(lower),
  );
}

/**
 * Execute a command by id. Checks enabled state, calls the handler,
 * records to operation history,
 * and publishes toast feedback.
 */
export async function executeCommand(
  id: string,
  ctx: CommandContext,
  input?: string,
): Promise<CommandResult> {
  const cmd = getCommand(id);
  if (!cmd) {
    return { success: false, message: `Unknown command: ${id}` };
  }

  if (!cmd.isEnabled(ctx)) {
    return { success: false, message: `Command not available: ${cmd.label}` };
  }

  const startTime = Date.now();
  let result: CommandResult;

  try {
    result = await cmd.execute(ctx, input);
  } catch (err: unknown) {
    if (isCancelledError(err)) {
      result = { success: false, cancelled: true, message: "Operation cancelled" };
    } else if (isStaleHunkError(err)) {
      result = {
        success: false,
        message: "File changed on disk since the diff was loaded. Please refresh and retry.",
      };
    } else {
      const message = err instanceof Error ? err.message : String(err);
      result = { success: false, message };
    }
  }

  // Dismissal and acknowledged cancellation are not command failures.
  // Admitted native transfers retain their actual receipts in Activity.
  if (result.cancelled) return result;
  const durationMs = Date.now() - startTime;

  // Record to operation history
  recordOperation({
    commandId: cmd.id,
    label: cmd.label,
    timestamp: startTime,
    durationMs,
    status: result.success ? "success" : "error",
    message: result.message,
  });

  // Publish toast feedback
  if (result.success) {
    publishToast({ kind: "success", title: cmd.label, body: result.message });
  } else {
    const isStaleHunk = result.message.includes("File changed on disk since the diff was loaded");
    publishToast({
      kind: isStaleHunk ? "warning" : "error",
      title: isStaleHunk ? "Stale diff" : `${cmd.label} failed`,
      body: result.message,
    });
  }

  return result;
}

// --- Subscription hooks ---

/**
 * Subscribe to registry changes. Returns an unsubscribe function.
 */
export function subscribeRegistry(callback: RegistrySubscriber): () => void {
  subscribers.add(callback);
  return () => {
    subscribers.delete(callback);
  };
}

/**
 * React hook that subscribes to the command registry.
 * Re-renders the component whenever commands are registered or unregistered.
 */
export function useCommands(): Command[] {
  const [snapshot, setSnapshot] = useState<Command[]>(commands);

  useEffect(() => {
    setSnapshot([...commands]);
    const sub: RegistrySubscriber = (next) => setSnapshot(next);
    subscribers.add(sub);
    return () => {
      subscribers.delete(sub);
    };
  }, []);

  return snapshot;
}
