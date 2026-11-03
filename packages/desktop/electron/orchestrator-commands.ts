/**
 * Slash Commands for the Crab Desktop Orchestrator.
 *
 * Manages slash command loading from multiple sources (built-in, repo-level,
 * user-level), listing, placeholder expansion, argument substitution, and
 * dynamic context injection.
 *
 * Inspired by Claude Code's skill/command system:
 * - YAML frontmatter for metadata (description, arguments, argument-hint)
 * - `$ARGUMENTS`, `$0`, `$1` positional substitution
 * - `!`command`` shell injection for dynamic context
 * - Nested directory structure: `commands/<name>/COMMAND.md`
 * - Precedence: builtin < repo < user (later overrides earlier)
 */

import * as fs from "node:fs";
import * as path from "node:path";
import * as os from "node:os";
import { execSync } from "node:child_process";

// ─── Types ───────────────────────────────────────────────────────────────────

export interface SlashCommand {
  name: string;
  description: string;
  template: string;
  source: "builtin" | "repo" | "user";
  path?: string;
  /** Hint shown in autocomplete for expected arguments, e.g. "[filename]". */
  argumentHint?: string;
  /** Named positional arguments for `$name` substitution. */
  arguments?: string[];
  /** Category for grouping in the dropdown (e.g. "git", "testing", "review"). */
  category?: string;
  /** When true, the command won't appear in the dropdown but can still be invoked. */
  hidden?: boolean;
  /** When true, the command runs in a forked/isolated context. */
  fork?: boolean;
}

/** Result of creating a new command file. */
export interface CreateCommandResult {
  success: boolean;
  path?: string;
  error?: string;
}

// ─── Built-in Commands ───────────────────────────────────────────────────────

const BUILTIN_COMMANDS: SlashCommand[] = [
  {
    name: "review",
    description: "Code review the current workspace diff",
    template:
      "Review the changes in this workspace. Focus on correctness, edge cases, error handling, and test coverage. Flag any issues as inline comments.",
    source: "builtin",
    category: "review",
  },
  {
    name: "fix-ci",
    description: "Fix failing CI checks",
    template:
      "The CI checks are failing. Read the failure logs, identify the root cause, and fix the issues. Run tests locally before pushing.",
    source: "builtin",
    category: "fix",
  },
  {
    name: "add-tests",
    description: "Add test coverage for recent changes",
    template:
      "Add comprehensive tests for the changes in this workspace. Cover happy paths, edge cases, and error conditions. Match the existing test style.",
    source: "builtin",
    category: "testing",
  },
  {
    name: "security-scan",
    description: "Security audit focusing on OWASP Top 10",
    template:
      "Perform a security audit of this workspace focusing on the OWASP Top 10 vulnerabilities. Check for injection flaws, broken authentication, sensitive data exposure, XXE, broken access control, security misconfigurations, XSS, insecure deserialization, known vulnerable components, and insufficient logging.",
    source: "builtin",
    category: "review",
  },
  {
    name: "explain",
    description: "Explain how a file or function works",
    argumentHint: "[file-or-symbol]",
    template:
      "Explain how $ARGUMENTS works. Walk through the logic step by step, noting any non-obvious design decisions, edge cases, or potential issues.",
    source: "builtin",
    category: "explore",
  },
  {
    name: "refactor",
    description: "Refactor code for clarity and maintainability",
    argumentHint: "[file-or-description]",
    template:
      "Refactor $ARGUMENTS for improved clarity, maintainability, and correctness. Preserve all existing behavior. Explain what you changed and why.",
    source: "builtin",
    category: "fix",
  },
  {
    name: "commit",
    description: "Stage and commit with a conventional message",
    template:
      "Look at the current git diff, stage the relevant changes, and create a commit with a clear conventional commit message. Group related changes logically.",
    source: "builtin",
    category: "git",
  },
  {
    name: "pr-description",
    description: "Generate a pull request description",
    template:
      "Generate a pull request description for the current branch's changes compared to main. Include: a summary of what changed, why it changed, how to test it, and any breaking changes or migration notes.",
    source: "builtin",
    category: "git",
  },
];

// ─── Frontmatter Parsing ─────────────────────────────────────────────────────

interface ParsedFrontmatter {
  description?: string;
  argumentHint?: string;
  arguments?: string[];
  category?: string;
  hidden?: boolean;
  fork?: boolean;
}

/**
 * Parses YAML-style frontmatter from a markdown command file.
 * Returns the parsed metadata and the remaining body content.
 */
function parseFrontmatter(content: string): { meta: ParsedFrontmatter; body: string } {
  const meta: ParsedFrontmatter = {};

  // Check for frontmatter delimiters.
  if (!content.startsWith("---")) {
    return { meta, body: content };
  }

  const endIdx = content.indexOf("\n---", 3);
  if (endIdx === -1) {
    return { meta, body: content };
  }

  const frontmatterBlock = content.slice(4, endIdx); // skip opening "---\n"
  const body = content.slice(endIdx + 4).trimStart(); // skip closing "---\n"

  // Simple YAML key-value parsing (no nested objects needed).
  for (const line of frontmatterBlock.split("\n")) {
    const trimmed = line.trim();
    if (!trimmed || trimmed.startsWith("#")) continue;

    const colonIdx = trimmed.indexOf(":");
    if (colonIdx === -1) continue;

    const key = trimmed.slice(0, colonIdx).trim();
    const rawValue = trimmed.slice(colonIdx + 1).trim();

    switch (key) {
      case "description":
        meta.description = stripQuotes(rawValue);
        break;
      case "argument-hint":
      case "argumentHint":
        meta.argumentHint = stripQuotes(rawValue);
        break;
      case "arguments":
        // Supports: "arg1 arg2" or "[arg1, arg2]"
        meta.arguments = parseListValue(rawValue);
        break;
      case "category":
        meta.category = stripQuotes(rawValue);
        break;
      case "hidden":
        meta.hidden = rawValue === "true";
        break;
      case "fork":
      case "context":
        meta.fork = rawValue === "fork" || rawValue === "true";
        break;
    }
  }

  return { meta, body };
}

function stripQuotes(s: string): string {
  if ((s.startsWith('"') && s.endsWith('"')) || (s.startsWith("'") && s.endsWith("'"))) {
    return s.slice(1, -1);
  }
  return s;
}

function parseListValue(raw: string): string[] {
  // Handle YAML list syntax: [a, b, c]
  if (raw.startsWith("[") && raw.endsWith("]")) {
    return raw
      .slice(1, -1)
      .split(",")
      .map((s) => stripQuotes(s.trim()))
      .filter(Boolean);
  }
  // Handle space-separated: "a b c"
  const unquoted = stripQuotes(raw);
  return unquoted.split(/\s+/).filter(Boolean);
}

// ─── Shell Injection ─────────────────────────────────────────────────────────

/**
 * Expands `!`command`` inline shell injections in a template.
 * Each occurrence is replaced with the stdout of the command.
 * Commands that fail are replaced with an error note.
 */
function expandShellInjections(template: string, cwd: string): string {
  // Match !`command` (inline single-line)
  return template.replace(/!\`([^`]+)\`/g, (_match, cmd: string) => {
    try {
      const output = execSync(cmd, {
        cwd,
        encoding: "utf-8",
        timeout: 10_000,
        maxBuffer: 1024 * 1024, // 1 MB
        stdio: ["pipe", "pipe", "pipe"],
      });
      return output.trimEnd();
    } catch (err: unknown) {
      const message = err instanceof Error ? err.message : String(err);
      return `[command failed: ${cmd} — ${message}]`;
    }
  });
}

// ─── Argument Substitution ───────────────────────────────────────────────────

/**
 * Replaces `$ARGUMENTS`, `$0`, `$1`, ..., and named `$argName` placeholders
 * in a template with the provided argument string.
 */
function substituteArguments(
  template: string,
  rawArgs: string,
  namedArgs?: string[],
): string {
  let result = template;

  // Split arguments respecting quoted strings.
  const positional = splitArguments(rawArgs);

  // $ARGUMENTS — full argument string
  result = result.replaceAll("$ARGUMENTS", rawArgs);

  // $ARGUMENTS[N] — indexed access
  result = result.replace(/\$ARGUMENTS\[(\d+)\]/g, (_m, idx) => {
    const i = parseInt(idx, 10);
    return positional[i] ?? "";
  });

  // $N shorthand — $0, $1, $2, ...
  result = result.replace(/\$(\d+)/g, (_m, idx) => {
    const i = parseInt(idx, 10);
    return positional[i] ?? "";
  });

  // Named arguments: $name where name is declared in frontmatter `arguments`
  if (namedArgs) {
    for (let i = 0; i < namedArgs.length; i++) {
      const name = namedArgs[i];
      const value = positional[i] ?? "";
      result = result.replaceAll(`$${name}`, value);
    }
  }

  return result;
}

/**
 * Splits an argument string respecting double-quoted groups.
 * "hello world" second → ["hello world", "second"]
 */
function splitArguments(raw: string): string[] {
  const args: string[] = [];
  let current = "";
  let inQuotes = false;

  for (let i = 0; i < raw.length; i++) {
    const ch = raw[i];
    if (ch === '"' && (i === 0 || raw[i - 1] !== "\\")) {
      inQuotes = !inQuotes;
    } else if (ch === " " && !inQuotes) {
      if (current) {
        args.push(current);
        current = "";
      }
    } else {
      current += ch;
    }
  }
  if (current) args.push(current);
  return args;
}

// ─── SlashCommandManager ─────────────────────────────────────────────────────

export class SlashCommandManager {
  private commands: SlashCommand[] = [];
  private repoPath: string = "";

  /**
   * Loads commands from all sources: built-in defaults, repo-level
   * (`.crab/orchestrator/commands/`), and user-level (`~/.crab/commands/`).
   *
   * Supports both flat files (`commands/name.md`) and nested directories
   * (`commands/name/COMMAND.md`).
   *
   * Repo and user commands with the same name as a built-in will override it.
   */
  loadCommands(repoPath: string): void {
    this.commands = [];
    this.repoPath = repoPath;

    // 1. Built-in commands (lowest precedence).
    this.commands.push(...BUILTIN_COMMANDS);

    // 2. Repo-level: .crab/orchestrator/commands/
    const repoDir = path.join(repoPath, ".crab", "orchestrator", "commands");
    this.loadFromDirectory(repoDir, "repo");

    // 3. User-level: ~/.crab/commands/
    const userDir = path.join(os.homedir(), ".crab", "commands");
    this.loadFromDirectory(userDir, "user");
  }

  /** Returns all loaded commands, optionally filtering hidden ones. */
  listCommands(includeHidden = false): SlashCommand[] {
    if (includeHidden) return [...this.commands];
    return this.commands.filter((c) => !c.hidden);
  }

  /** Returns distinct categories across all visible commands. */
  listCategories(): string[] {
    const cats = new Set<string>();
    for (const cmd of this.commands) {
      if (!cmd.hidden && cmd.category) cats.add(cmd.category);
    }
    return [...cats].sort();
  }

  /**
   * Expands a command template with full argument substitution and
   * optional shell injection.
   *
   * @param name - The command name (without leading `/`).
   * @param args - Key-value args for legacy `{placeholder}` expansion,
   *               OR a `_raw` key containing the full argument string
   *               for `$ARGUMENTS`-style substitution.
   */
  expand(name: string, args: Record<string, string>): string {
    const cmd = this.commands.find((c) => c.name === name);
    if (!cmd) {
      throw new Error(`Unknown command: /${name}`);
    }

    let expanded = cmd.template;

    // If `_raw` is provided, use the new $ARGUMENTS substitution.
    const rawArgs = args._raw ?? "";
    if (rawArgs || expanded.includes("$ARGUMENTS") || expanded.match(/\$\d/)) {
      expanded = substituteArguments(expanded, rawArgs, cmd.arguments);
    }

    // Legacy {placeholder} expansion for backward compatibility.
    for (const [key, value] of Object.entries(args)) {
      if (key === "_raw") continue;
      expanded = expanded.replaceAll(`{${key}}`, value);
    }

    // Shell injection: expand !`command` blocks.
    const cwd = cmd.path ? path.dirname(cmd.path) : this.repoPath;
    if (expanded.includes("!`")) {
      expanded = expandShellInjections(expanded, cwd || process.cwd());
    }

    return expanded;
  }

  /**
   * Creates a new command file at the specified scope.
   *
   * @param name - Command name (used as filename).
   * @param scope - "repo" writes to `.crab/orchestrator/commands/`,
   *                "user" writes to `~/.crab/commands/`.
   * @param content - Full markdown content (including optional frontmatter).
   */
  createCommand(
    name: string,
    scope: "repo" | "user",
    content: string,
  ): CreateCommandResult {
    const dir =
      scope === "repo"
        ? path.join(this.repoPath, ".crab", "orchestrator", "commands")
        : path.join(os.homedir(), ".crab", "commands");

    try {
      fs.mkdirSync(dir, { recursive: true });
      const filePath = path.join(dir, `${name}.md`);

      if (fs.existsSync(filePath)) {
        return { success: false, error: `Command /${name} already exists at ${filePath}` };
      }

      fs.writeFileSync(filePath, content, "utf-8");

      // Reload so the new command is immediately available.
      this.loadCommands(this.repoPath);

      return { success: true, path: filePath };
    } catch (err: unknown) {
      const message = err instanceof Error ? err.message : String(err);
      return { success: false, error: message };
    }
  }

  /**
   * Deletes a user or repo command by name.
   * Built-in commands cannot be deleted.
   */
  deleteCommand(name: string): { success: boolean; error?: string } {
    const cmd = this.commands.find((c) => c.name === name);
    if (!cmd) return { success: false, error: `Command /${name} not found` };
    if (cmd.source === "builtin") return { success: false, error: "Cannot delete built-in commands" };
    if (!cmd.path) return { success: false, error: "Command has no file path" };

    try {
      fs.unlinkSync(cmd.path);
      this.loadCommands(this.repoPath);
      return { success: true };
    } catch (err: unknown) {
      const message = err instanceof Error ? err.message : String(err);
      return { success: false, error: message };
    }
  }

  /**
   * Returns the raw content of a command file (for editing in the UI).
   */
  readCommandSource(name: string): { success: boolean; content?: string; path?: string; error?: string } {
    const cmd = this.commands.find((c) => c.name === name);
    if (!cmd) return { success: false, error: `Command /${name} not found` };
    if (cmd.source === "builtin") {
      // Synthesize content for built-in commands.
      const lines = [
        "---",
        `description: "${cmd.description}"`,
        cmd.argumentHint ? `argument-hint: "${cmd.argumentHint}"` : null,
        cmd.category ? `category: ${cmd.category}` : null,
        "---",
        "",
        cmd.template,
      ].filter((l) => l !== null);
      return { success: true, content: lines.join("\n") };
    }
    if (!cmd.path) return { success: false, error: "Command has no file path" };

    try {
      const content = fs.readFileSync(cmd.path, "utf-8");
      return { success: true, content, path: cmd.path };
    } catch (err: unknown) {
      const message = err instanceof Error ? err.message : String(err);
      return { success: false, error: message };
    }
  }

  /**
   * Generates a scaffold for a new command with frontmatter.
   */
  scaffoldCommand(opts: {
    name: string;
    description: string;
    argumentHint?: string;
    category?: string;
  }): string {
    const lines: string[] = ["---"];
    lines.push(`description: "${opts.description}"`);
    if (opts.argumentHint) lines.push(`argument-hint: "${opts.argumentHint}"`);
    if (opts.category) lines.push(`category: ${opts.category}`);
    lines.push("---", "", "");
    lines.push(`# Instructions for /${opts.name}`, "");
    lines.push("$ARGUMENTS");
    return lines.join("\n");
  }

  // ─── Private Helpers ─────────────────────────────────────────────────────

  /**
   * Loads command files from a directory. Supports:
   * - Flat files: `dir/name.md`
   * - Nested directories: `dir/name/COMMAND.md`
   *
   * Commands loaded later override earlier ones with the same name.
   */
  private loadFromDirectory(dir: string, source: "repo" | "user"): void {
    try {
      if (!fs.existsSync(dir)) return;

      const entries = fs.readdirSync(dir, { withFileTypes: true });

      for (const entry of entries) {
        try {
          if (entry.isFile() && entry.name.endsWith(".md")) {
            // Flat file: name.md
            const filePath = path.join(dir, entry.name);
            this.loadCommandFile(filePath, path.basename(entry.name, ".md"), source);
          } else if (entry.isDirectory()) {
            // Nested directory: name/COMMAND.md
            const commandMd = path.join(dir, entry.name, "COMMAND.md");
            if (fs.existsSync(commandMd)) {
              this.loadCommandFile(commandMd, entry.name, source);
            }
          }
        } catch {
          // Skip individual malformed entries.
        }
      }
    } catch {
      // If the directory can't be read, skip silently.
    }
  }

  /**
   * Loads a single command file, parsing frontmatter and extracting metadata.
   */
  private loadCommandFile(filePath: string, name: string, source: "repo" | "user"): void {
    const content = fs.readFileSync(filePath, "utf-8");
    const { meta, body } = parseFrontmatter(content);

    const description = meta.description || this.extractDescription(body);
    const template = body || content;

    const command: SlashCommand = {
      name,
      description,
      template,
      source,
      path: filePath,
      argumentHint: meta.argumentHint,
      arguments: meta.arguments,
      category: meta.category,
      hidden: meta.hidden,
      fork: meta.fork,
    };

    // Override any existing command with the same name.
    const existingIdx = this.commands.findIndex((c) => c.name === name);
    if (existingIdx !== -1) {
      this.commands[existingIdx] = command;
    } else {
      this.commands.push(command);
    }
  }

  /**
   * Extracts the description from a command file's body content.
   * Skips markdown headings and returns the first non-empty prose line.
   */
  private extractDescription(content: string): string {
    const lines = content.split("\n");
    for (const line of lines) {
      const trimmed = line.trim();
      if (!trimmed) continue;
      // Skip markdown headings — they're structural, not descriptive.
      if (trimmed.startsWith("#")) continue;
      // Truncate long descriptions.
      return trimmed.length > 100 ? trimmed.slice(0, 97) + "…" : trimmed;
    }
    return "";
  }
}
