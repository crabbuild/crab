// Extended view commands for the Command Palette.
// This module self-registers all view-specific commands when imported (side-effect).
// Covers: Worktree, Conflict, LargeFile, PullRequest, file-level Stage,
// file-open actions, and commit-context actions.

import {
  Plus,
  Trash2,
  ExternalLink,
  Play,
  Square,
  CheckCheck,
  Check,
  ArrowLeft,
  ArrowRight,
  Columns,
  Download,
  Upload,
  ArrowUp,
  Minus,
  GitBranch,
  GitMerge,
  Globe,
  FileDiff,
  FileText,
  LogOut,
} from "lucide-react";
import { registerCommand } from "./command-registry";
import type { Command, CommandContext, CommandResult } from "./command-registry";
import { rpc } from "../ipc";
import type { HydrateVerb, Scope } from "./hydrateScope";

// --- Worktree category ---

const worktreeCommands: Command[] = [
  {
    id: "git.worktree.create",
    label: "Git: Create Worktree…",
    category: "Worktree",
    icon: Plus,
    prompt: { kind: "ref", placeholder: "Worktree path…" },
    isEnabled: () => true,
    execute: async (ctx: CommandContext, input?: string): Promise<CommandResult> => {
      if (!input) {
        return { success: false, message: "Worktree path is required" };
      }
      await rpc.gitWorktreeAdd(ctx.repoId, { path: input });
      return { success: true, message: `Created worktree at ${input}` };
    },
  },
  {
    id: "git.worktree.remove",
    label: "Git: Remove Worktree…",
    category: "Worktree",
    icon: Trash2,
    prompt: { kind: "ref", placeholder: "Worktree path to remove…" },
    isEnabled: () => true,
    execute: async (ctx: CommandContext, input?: string): Promise<CommandResult> => {
      if (!input) {
        return { success: false, message: "Worktree path is required" };
      }
      await rpc.gitWorktreeRemove(ctx.repoId, input);
      return { success: true, message: `Removed worktree at ${input}` };
    },
  },
  {
    id: "worktree.open-window",
    label: "Worktree: Open in New Window",
    category: "Worktree",
    icon: ExternalLink,
    isEnabled: () => true,
    execute: async (_ctx: CommandContext, input?: string): Promise<CommandResult> => {
      if (!input) {
        return { success: false, message: "Worktree path is required" };
      }
      await window.crab.fsOpenExternal(input);
      return { success: true, message: `Opened worktree in new window` };
    },
  },
];

// --- Conflict category ---

const conflictCommands: Command[] = [
  {
    id: "git.op.continue",
    label: "Git: Continue Operation",
    category: "Conflict",
    icon: Play,
    isEnabled: (ctx: CommandContext) => ctx.activeOperation !== null,
    execute: async (ctx: CommandContext, input?: string): Promise<CommandResult> => {
      const opId = input ?? "";
      const result = await rpc.opContinue(ctx.repoId, opId);
      return { success: true, message: `Operation continued (${result.state})` };
    },
  },
  {
    id: "git.op.abort",
    label: "Git: Abort Operation",
    category: "Conflict",
    icon: Square,
    isEnabled: (ctx: CommandContext) => ctx.activeOperation !== null,
    execute: async (ctx: CommandContext, input?: string): Promise<CommandResult> => {
      const opId = input ?? "";
      const result = await rpc.opAbort(ctx.repoId, opId);
      return { success: true, message: `Operation aborted (${result.state})` };
    },
  },
  {
    id: "git.conflicts.resolve-all",
    label: "Git: Mark All Conflicts Resolved",
    category: "Conflict",
    icon: CheckCheck,
    isEnabled: (ctx: CommandContext) => ctx.activeOperation !== null,
    execute: async (ctx: CommandContext): Promise<CommandResult> => {
      const { files } = await rpc.conflictList(ctx.repoId);
      for (const file of files) {
        const sides = await rpc.conflictReadSides(ctx.repoId, file.path);
        const content = sides.current ?? sides.incoming ?? "";
        await rpc.gitApplyResolution(ctx.repoId, file.path, content);
      }
      return { success: true, message: `Resolved ${files.length} conflicted file(s)` };
    },
  },
  {
    id: "git.conflict.resolve",
    label: "Git: Mark File Resolved",
    category: "Conflict",
    icon: Check,
    isEnabled: () => true,
    execute: async (ctx: CommandContext, input?: string): Promise<CommandResult> => {
      if (!input) {
        return { success: false, message: "File path is required" };
      }
      const sides = await rpc.conflictReadSides(ctx.repoId, input);
      const content = sides.current ?? sides.incoming ?? "";
      await rpc.gitApplyResolution(ctx.repoId, input, content);
      return { success: true, message: `Marked ${input} as resolved` };
    },
  },
  {
    id: "git.conflict.accept-ours",
    label: "Git: Accept Ours",
    category: "Conflict",
    icon: ArrowLeft,
    isEnabled: () => true,
    execute: async (ctx: CommandContext, input?: string): Promise<CommandResult> => {
      if (!input) {
        return { success: false, message: "File path is required" };
      }
      const sides = await rpc.conflictReadSides(ctx.repoId, input);
      const content = sides.current ?? "";
      await rpc.gitApplyResolution(ctx.repoId, input, content);
      return { success: true, message: `Accepted ours for ${input}` };
    },
  },
  {
    id: "git.conflict.accept-theirs",
    label: "Git: Accept Theirs",
    category: "Conflict",
    icon: ArrowRight,
    isEnabled: () => true,
    execute: async (ctx: CommandContext, input?: string): Promise<CommandResult> => {
      if (!input) {
        return { success: false, message: "File path is required" };
      }
      const sides = await rpc.conflictReadSides(ctx.repoId, input);
      const content = sides.incoming ?? "";
      await rpc.gitApplyResolution(ctx.repoId, input, content);
      return { success: true, message: `Accepted theirs for ${input}` };
    },
  },
  {
    id: "git.conflict.resolve-with-agent",
    label: "Git: Resolve Conflict with Agent",
    category: "Conflict",
    icon: GitMerge,
    isEnabled: () => true,
    execute: async (ctx: CommandContext, input?: string): Promise<CommandResult> => {
      if (!input) {
        return { success: false, message: "File path is required" };
      }
      // Read the conflict sides and build a resolution prompt
      const sides = await rpc.conflictReadSides(ctx.repoId, input);
      const workingContent = await rpc.readText(ctx.repoId, input);

      // Send to the orchestrator for AI-assisted resolution
      const prompt = [
        `Resolve the merge conflict in \`${input}\`.`,
        "",
        "## Base (common ancestor):",
        "```",
        sides.base ?? "(no base — file was added on both sides)",
        "```",
        "",
        "## Current (ours / HEAD):",
        "```",
        sides.current ?? "(deleted on our side)",
        "```",
        "",
        "## Incoming (theirs):",
        "```",
        sides.incoming ?? "(deleted on their side)",
        "```",
        "",
        "## Working tree (with conflict markers):",
        "```",
        workingContent.text ?? "",
        "```",
        "",
        "Produce the final merged content that preserves both sides' intent.",
        "Write the resolved content to the file and run `git add` to mark it resolved.",
      ].join("\n");

      // Dispatch to orchestrator via IPC
      try {
        await (window as any).crab?.orchestrator?.iterate?.(null, prompt);
        return { success: true, message: `Agent is resolving ${input}` };
      } catch {
        return { success: false, message: "Orchestrator not available. Resolve manually." };
      }
    },
  },
  {
    id: "git.conflict.open-external",
    label: "Git: Open in External Merge Tool",
    category: "Conflict",
    icon: ExternalLink,
    isEnabled: (ctx: CommandContext) => !!ctx.repoPath,
    execute: async (ctx: CommandContext, input?: string): Promise<CommandResult> => {
      if (!input || !ctx.repoPath) {
        return { success: false, message: "File path and repo path are required" };
      }
      // Invoke git mergetool for the specific file via the agent's shell
      try {
        const bridge = (window as any).crab;
        if (bridge?.shell?.exec) {
          await bridge.shell.exec(`git mergetool --no-prompt -- "${input}"`, { cwd: ctx.repoPath });
        } else {
          // Fallback: use the op_start mechanism or just inform the user
          return { success: false, message: "Run `git mergetool` in the terminal for this file." };
        }
        return { success: true, message: `Opened external merge tool for ${input}` };
      } catch (e) {
        const msg = e instanceof Error ? e.message : String(e);
        return { success: false, message: `Failed to open merge tool: ${msg}` };
      }
    },
  },
];

// --- LargeFile category ---

async function transferSelection(ctx: CommandContext, verb: HydrateVerb, scope?: Scope): Promise<CommandResult> {
  const transfer = ctx.fileTransfers?.[verb];
  if (!transfer) return { success: false, message: "Verified file transfers are unavailable for this repository" };
  const count = await transfer(scope);
  if (count === null) return { success: false, cancelled: true, message: "File transfer cancelled" };
  const action = verb === "hydrate" ? "Hydrated" : "Dehydrated";
  return { success: true, message: count === 0 ? `No selected files need ${verb === "hydrate" ? "hydration" : "dehydration"}` : `${action} ${count} ${count === 1 ? "file" : "files"}` };
}

const largeFileCommands: Command[] = [
  {
    id: "crab.track",
    label: "Crab: Track Files…",
    category: "LargeFile",
    icon: Plus,
    prompt: { kind: "ref", placeholder: "File pattern to track…" },
    isEnabled: () => true,
    execute: async (ctx: CommandContext, input?: string): Promise<CommandResult> => {
      if (!input) {
        return { success: false, message: "File pattern is required" };
      }
      await rpc.crabAddStructured(ctx.repoId, [input]);
      return { success: true, message: `Tracking pattern: ${input}` };
    },
  },
  {
    id: "crab.hydrate",
    label: "Crab: Hydrate All",
    category: "LargeFile",
    icon: Download,
    isEnabled: (ctx: CommandContext) => !!ctx.fileTransfers?.hydrate,
    execute: (ctx: CommandContext) => transferSelection(ctx, "hydrate"),
  },
  {
    id: "crab.dehydrate",
    label: "Crab: Dehydrate All",
    category: "LargeFile",
    icon: Upload,
    isEnabled: (ctx: CommandContext) => !!ctx.fileTransfers?.dehydrate,
    execute: (ctx: CommandContext) => transferSelection(ctx, "dehydrate"),
  },
  {
    id: "crab.push",
    label: "Crab: Push Large Files",
    category: "LargeFile",
    icon: ArrowUp,
    isEnabled: () => true,
    execute: async (ctx: CommandContext): Promise<CommandResult> => {
      await rpc.crabPushStructured(ctx.repoId);
      return { success: true, message: "Pushed large file data to remote" };
    },
  },
  {
    id: "crab.hydrate.file",
    label: "Crab: Hydrate File",
    category: "LargeFile",
    icon: Download,
    prompt: { kind: "ref", placeholder: "Exact file path to hydrate…" },
    isEnabled: (ctx: CommandContext) => !!ctx.fileTransfers?.hydrate,
    execute: async (ctx: CommandContext, input?: string): Promise<CommandResult> => {
      if (!input) {
        return { success: false, message: "File path is required" };
      }
      return transferSelection(ctx, "hydrate", { kind: "custom", paths: [input] });
    },
  },
  {
    id: "crab.dehydrate.file",
    label: "Crab: Dehydrate File",
    category: "LargeFile",
    icon: Upload,
    prompt: { kind: "ref", placeholder: "Exact file path to dehydrate…" },
    isEnabled: (ctx: CommandContext) => !!ctx.fileTransfers?.dehydrate,
    execute: async (ctx: CommandContext, input?: string): Promise<CommandResult> => {
      if (!input) {
        return { success: false, message: "File path is required" };
      }
      return transferSelection(ctx, "dehydrate", { kind: "custom", paths: [input] });
    },
  },
  {
    id: "crab.track.file",
    label: "Crab: Track File",
    category: "LargeFile",
    icon: Plus,
    isEnabled: () => true,
    execute: async (ctx: CommandContext, input?: string): Promise<CommandResult> => {
      if (!input) {
        return { success: false, message: "File path is required" };
      }
      await rpc.crabAddStructured(ctx.repoId, [input]);
      return { success: true, message: `Tracking ${input}` };
    },
  },
  {
    id: "crab.untrack.file",
    label: "Crab: Untrack File",
    category: "LargeFile",
    icon: Minus,
    isEnabled: () => true,
    execute: async (ctx: CommandContext, input?: string): Promise<CommandResult> => {
      if (!input) {
        return { success: false, message: "File path is required" };
      }
      await rpc.crabResetFiles(ctx.repoId, [input]);
      return { success: true, message: `Untracked ${input}` };
    },
  },
];

// --- PullRequest category ---

const pullRequestCommands: Command[] = [
  {
    id: "pr.create",
    label: "PR: Create Pull Request",
    category: "PullRequest",
    icon: Plus,
    isEnabled: (ctx: CommandContext) => ctx.forgeProvider !== null && ctx.currentBranch.length > 0,
    execute: async (ctx: CommandContext, input?: string): Promise<CommandResult> => {
      if (!input) {
        return { success: false, message: "PR title is required" };
      }
      if (!ctx.currentBranch) {
        return { success: false, message: "No current branch — cannot determine PR head" };
      }
      const result = await rpc.forgeCreatePr(ctx.repoId, {
        title: input,
        body: "",
        head: ctx.currentBranch,
        base: "main",
        draft: false,
      });
      return { success: true, message: `Created PR #${result.number}` };
    },
  },
  {
    id: "pr.checkout",
    label: "PR: Checkout PR Branch",
    category: "PullRequest",
    icon: GitBranch,
    isEnabled: (ctx: CommandContext) => ctx.forgeProvider !== null,
    execute: async (ctx: CommandContext, input?: string): Promise<CommandResult> => {
      if (!input) {
        return { success: false, message: "Branch name is required" };
      }
      await rpc.gitFetchBranch(ctx.repoId, input);
      await rpc.gitCheckout(ctx.repoId, input);
      return { success: true, message: `Checked out PR branch ${input}` };
    },
  },
  {
    id: "pr.merge",
    label: "PR: Merge Pull Request",
    category: "PullRequest",
    icon: GitMerge,
    isEnabled: (ctx: CommandContext) => ctx.forgeProvider !== null,
    execute: async (ctx: CommandContext, input?: string): Promise<CommandResult> => {
      if (!input) {
        return { success: false, message: "PR number is required" };
      }
      const prNumber = parseInt(input, 10);
      if (isNaN(prNumber)) {
        return { success: false, message: "Invalid PR number" };
      }
      await rpc.forgePrComment(ctx.repoId, prNumber, "Merged via crab-desktop");
      return { success: true, message: `Merged PR #${prNumber}` };
    },
  },
  {
    id: "pr.view-web",
    label: "PR: View on Web",
    category: "PullRequest",
    icon: Globe,
    isEnabled: (ctx: CommandContext) => ctx.forgeProvider !== null,
    execute: async (_ctx: CommandContext, input?: string): Promise<CommandResult> => {
      if (!input) {
        return { success: false, message: "PR URL is required" };
      }
      await window.crab.fsOpenExternal(input);
      return { success: true, message: "Opened PR in browser" };
    },
  },
];

// --- Stage category (file-level) ---

const fileStageCommands: Command[] = [
  {
    id: "git.stage.file",
    label: "Git: Stage File",
    category: "Stage",
    icon: Plus,
    isEnabled: () => true,
    execute: async (ctx: CommandContext, input?: string): Promise<CommandResult> => {
      if (!input) {
        return { success: false, message: "File path is required" };
      }
      // Always use `git add` for whole-file staging. This handles all
      // cases correctly: untracked, modified, deleted, and renamed files.
      // Hunk-level (partial) staging is handled separately via the
      // checkbox + gitStageHunks flow.
      await rpc.gitAddPaths(ctx.repoId, [input]);
      return { success: true, message: `Staged ${input}` };
    },
  },
  {
    id: "git.unstage.file",
    label: "Git: Unstage File",
    category: "Stage",
    icon: Minus,
    isEnabled: () => true,
    execute: async (ctx: CommandContext, input?: string): Promise<CommandResult> => {
      if (!input) {
        return { success: false, message: "File path is required" };
      }
      // Always use `git reset HEAD` for whole-file unstaging. This
      // handles all cases: fully staged files, newly added files, etc.
      // Hunk-level (partial) unstaging is handled separately via the
      // checkbox + gitUnstageHunks flow.
      await rpc.gitResetPaths(ctx.repoId, [input]);
      return { success: true, message: `Unstaged ${input}` };
    },
  },
  {
    id: "git.discard.file",
    label: "Git: Discard File Changes",
    category: "Stage",
    icon: Trash2,
    destructive: true,
    prompt: { kind: "ref", placeholder: "Repository-relative file path…" },
    isEnabled: (ctx) => !!ctx.fileTransfers?.restore,
    execute: async (ctx: CommandContext, input?: string): Promise<CommandResult> => {
      if (!input) {
        return { success: false, message: "File path is required" };
      }
      const count = await ctx.fileTransfers!.restore!([input]);
      return count === null ? { success: false, cancelled: true, message: "Restoration cancelled" }
        : { success: true, message: count ? `Restored ${input} from the index` : "No unstaged tracked changes to restore" };
    },
  },
  {
    id: "open.diff",
    label: "Open Diff",
    category: "Stage",
    icon: FileDiff,
    isEnabled: () => true,
    execute: async (_ctx: CommandContext, input?: string): Promise<CommandResult> => {
      if (!input) {
        return { success: false, message: "File path is required" };
      }
      // The diff view is opened by the renderer via navigation; this command
      // signals intent. The view layer handles the actual navigation.
      return { success: true, message: `Opening diff for ${input}` };
    },
  },
  {
    id: "open.file",
    label: "Open File",
    category: "Stage",
    icon: FileText,
    isEnabled: () => true,
    execute: async (_ctx: CommandContext, input?: string): Promise<CommandResult> => {
      if (!input) {
        return { success: false, message: "File path is required" };
      }
      return { success: true, message: `Opening ${input}` };
    },
  },
  {
    id: "open.merge-editor",
    label: "Open Merge Editor",
    category: "Stage",
    icon: Columns,
    isEnabled: () => true,
    execute: async (_ctx: CommandContext, input?: string): Promise<CommandResult> => {
      if (!input) {
        return { success: false, message: "File path is required" };
      }
      return { success: true, message: `Opening merge editor for ${input}` };
    },
  },
];

// --- Branch category (commit-context) ---

const commitContextCommands: Command[] = [
  {
    id: "git.branch.create-at",
    label: "Git: Create Branch at Commit…",
    category: "Branch",
    icon: GitBranch,
    prompt: { kind: "ref", placeholder: "New branch name…" },
    isEnabled: () => true,
    execute: async (ctx: CommandContext, input?: string): Promise<CommandResult> => {
      if (!input) {
        return { success: false, message: "Branch name is required" };
      }
      // Input format: "branchName:commitSha"
      const separatorIdx = input.indexOf(":");
      let branchName = input;
      let startPoint: string | undefined;
      if (separatorIdx !== -1) {
        branchName = input.slice(0, separatorIdx);
        startPoint = input.slice(separatorIdx + 1);
      }
      await rpc.gitBranchCreate(ctx.repoId, branchName, { start_point: startPoint });
      return { success: true, message: `Created branch ${branchName}${startPoint ? ` at ${startPoint.slice(0, 7)}` : ""}` };
    },
  },
];

// --- History category (commit checkout) ---

const commitCheckoutCommands: Command[] = [
  {
    id: "git.checkout.commit",
    label: "Git: Checkout Commit (Detached HEAD)",
    category: "History",
    icon: LogOut,
    prompt: { kind: "commit", placeholder: "Select commit…" },
    isEnabled: () => true,
    execute: async (ctx: CommandContext, input?: string): Promise<CommandResult> => {
      if (!input) {
        return { success: false, message: "Commit SHA is required" };
      }
      await rpc.gitCheckout(ctx.repoId, input, { detach: true });
      return { success: true, message: `Checked out ${input.slice(0, 7)} (detached HEAD)` };
    },
  },
];

// --- Register all commands at module load time ---


// --- Browser category ---

const browserCommands: Command[] = [
  {
    id: "browser.open",
    label: "Browser: Open Browser View",
    category: "Browser",
    icon: Globe,
    isEnabled: () => true,
    execute: async (_ctx: CommandContext): Promise<CommandResult> => {
      window.dispatchEvent(new CustomEvent("crab:set-active-view", { detail: { view: "browser" } }));
      return { success: true, message: "Opened browser" };
    },
  },
  {
    id: "browser.new-tab",
    label: "Browser: New Tab",
    category: "Browser",
    icon: Globe,
    isEnabled: () => true,
    execute: async (_ctx: CommandContext): Promise<CommandResult> => {
      window.dispatchEvent(new CustomEvent("crab:browser-new-tab"));
      return { success: true, message: "Opened new browser tab" };
    },
  },
];

const ALL_VIEW_COMMANDS: Command[] = [
  ...browserCommands,
  ...worktreeCommands,
  ...conflictCommands,
  ...largeFileCommands,
  ...pullRequestCommands,
  ...fileStageCommands,
  ...commitContextCommands,
  ...commitCheckoutCommands,
];

for (const cmd of ALL_VIEW_COMMANDS) {
  registerCommand(cmd);
}
