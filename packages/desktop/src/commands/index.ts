/**
 * Central command registry for Crab Desktop.
 *
 * Every action in the app registers a Command here. The command palette,
 * keybinding service, and context menus all resolve against this registry
 * rather than coupling directly to UI elements.
 */

// Context passed to every command's `run` function.
export interface CommandCtx {
  repoId?: string;
}

// A registered command.
export interface Command {
  id: string;
  title: string;
  category: string;
  keywords?: string[];
  when?: () => boolean;
  run: (ctx: CommandCtx) => Promise<void>;
}

const RECENT_STORAGE_KEY = "crab:recent-commands";
const MAX_RECENT = 10;

export class CommandRegistry {
  private commands = new Map<string, Command>();
  private recentIds: string[] = [];

  constructor() {
    this.recentIds = this.loadRecent();
  }

  /** Register a command. Overwrites any existing command with the same id. */
  register(command: Command): void {
    this.commands.set(command.id, command);
  }

  /** Remove a command by id. No-op if the id isn't registered. */
  unregister(id: string): void {
    this.commands.delete(id);
  }

  /** Return all registered commands. */
  getAll(): Command[] {
    return Array.from(this.commands.values());
  }

  /** Look up a single command by id. */
  getById(id: string): Command | undefined {
    return this.commands.get(id);
  }

  /** Return only commands whose `when` predicate is absent or returns true. */
  getAvailable(): Command[] {
    return this.getAll().filter((cmd) => !cmd.when || cmd.when());
  }

  /** Return recently-run commands (most recent first), filtered to currently registered commands. */
  getRecent(): Command[] {
    const result: Command[] = [];
    for (const id of this.recentIds) {
      const cmd = this.commands.get(id);
      if (cmd) result.push(cmd);
    }
    return result;
  }

  /** Execute a command by id. Throws if the command is not found. */
  async execute(id: string, ctx: CommandCtx): Promise<void> {
    const cmd = this.commands.get(id);
    if (!cmd) {
      throw new Error(`Command not found: ${id}`);
    }
    this.pushRecent(id);
    await cmd.run(ctx);
  }

  /** Push a command id to the front of the recent list. */
  private pushRecent(id: string): void {
    this.recentIds = [id, ...this.recentIds.filter((r) => r !== id)].slice(0, MAX_RECENT);
    this.persistRecent();
  }

  private loadRecent(): string[] {
    try {
      const raw = sessionStorage.getItem(RECENT_STORAGE_KEY);
      if (!raw) return [];
      const parsed = JSON.parse(raw);
      if (Array.isArray(parsed)) return parsed.slice(0, MAX_RECENT);
    } catch {
      // Ignore parse errors — start fresh.
    }
    return [];
  }

  private persistRecent(): void {
    try {
      sessionStorage.setItem(RECENT_STORAGE_KEY, JSON.stringify(this.recentIds));
    } catch {
      // sessionStorage may be unavailable in some contexts.
    }
  }
}

/** Global singleton — importable from anywhere in the app. */
export const commands = new CommandRegistry();
