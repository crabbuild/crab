// Built-in git commands for the Command Palette.
// This module self-registers all commands when imported (side-effect).

import {
  ArrowDown,
  ArrowUp,
  GitCommitHorizontal,
  RefreshCw,
} from "lucide-react";
import { registerCommand } from "./command-registry";
import type { Command, CommandContext, CommandResult } from "./command-registry";
import { rpc } from "../ipc";

// --- Commit category ---

const commitCommands: Command[] = [
  {
    id: "git.commit",
    label: "Git: Commit",
    category: "Commit",
    icon: GitCommitHorizontal,
    shortcut: "⌘↵",
    isEnabled: (ctx: CommandContext) => ctx.hasStagedChanges,
    execute: async (ctx: CommandContext, input?: string): Promise<CommandResult> => {
      const message = input ?? "";
      if (!message) {
        return { success: false, message: "Commit message is required" };
      }
      const result = await rpc.gitCommit(ctx.repoId, { message });
      return { success: true, message: `Committed ${result.sha.slice(0, 7)}: ${result.subject}` };
    },
  },
  {
    id: "git.commit.amend",
    label: "Git: Commit (Amend)",
    category: "Commit",
    isEnabled: (ctx: CommandContext) => ctx.hasStagedChanges || ctx.dirtyCount >= 0,
    execute: async (ctx: CommandContext, input?: string): Promise<CommandResult> => {
      const result = await rpc.gitCommit(ctx.repoId, {
        message: input ?? "",
        amend: true,
      });
      return { success: true, message: `Amended ${result.sha.slice(0, 7)}: ${result.subject}` };
    },
  },
  {
    id: "git.commit.signoff",
    label: "Git: Commit (Sign-off)",
    category: "Commit",
    isEnabled: (ctx: CommandContext) => ctx.hasStagedChanges,
    execute: async (ctx: CommandContext, input?: string): Promise<CommandResult> => {
      const message = input ?? "";
      if (!message) {
        return { success: false, message: "Commit message is required" };
      }
      const result = await rpc.gitCommit(ctx.repoId, {
        message,
        sign_off: true,
      });
      return { success: true, message: `Committed (signed-off) ${result.sha.slice(0, 7)}: ${result.subject}` };
    },
  },
];


// --- Sync category ---

const syncCommands: Command[] = [
  {
    id: "git.push",
    label: "Git: Push",
    category: "Sync",
    icon: ArrowUp,
    shortcut: "⌘⇧K",
    isEnabled: () => true,
    execute: async (ctx: CommandContext): Promise<CommandResult> => {
      await rpc.gitPushBranch(ctx.repoId, ctx.currentBranch || "HEAD");
      return { success: true, message: `Pushed to ${ctx.upstream ?? "remote"}` };
    },
  },
  {
    id: "git.push.force-lease",
    label: "Git: Push (Force with Lease)",
    category: "Sync",
    isEnabled: () => true,
    execute: async (ctx: CommandContext): Promise<CommandResult> => {
      await rpc.gitPushRemote(ctx.repoId, ctx.currentBranch || "HEAD", { force_with_lease: true });
      return { success: true, message: `Force-pushed (with lease) to ${ctx.upstream ?? "remote"}` };
    },
  },
  {
    id: "git.pull",
    label: "Git: Pull",
    category: "Sync",
    icon: ArrowDown,
    shortcut: "⌘⇧L",
    isEnabled: () => true,
    execute: async (ctx: CommandContext): Promise<CommandResult> => {
      await rpc.gitPull(ctx.repoId);
      return { success: true, message: "Pulled latest changes" };
    },
  },
  {
    id: "git.pull.rebase",
    label: "Git: Pull (Rebase)",
    category: "Sync",
    isEnabled: () => true,
    execute: async (ctx: CommandContext): Promise<CommandResult> => {
      await rpc.gitPull(ctx.repoId, { rebase: true });
      return { success: true, message: "Pulled with rebase" };
    },
  },
  {
    id: "git.fetch",
    label: "Git: Fetch",
    category: "Sync",
    icon: RefreshCw,
    isEnabled: () => true,
    execute: async (ctx: CommandContext): Promise<CommandResult> => {
      await rpc.gitFetch(ctx.repoId);
      return { success: true, message: "Fetched from remote" };
    },
  },
  {
    id: "git.fetch.all",
    label: "Git: Fetch All Remotes",
    category: "Sync",
    isEnabled: () => true,
    execute: async (ctx: CommandContext): Promise<CommandResult> => {
      await rpc.gitFetchAll(ctx.repoId);
      return { success: true, message: "Fetched from all remotes" };
    },
  },
];


// --- Branch category ---

const branchCommands: Command[] = [
  {
    id: "git.checkout",
    label: "Git: Checkout Branch…",
    category: "Branch",
    prompt: { kind: "branch", placeholder: "Select branch…" },
    isEnabled: () => true,
    execute: async (ctx: CommandContext, input?: string): Promise<CommandResult> => {
      if (!input) {
        return { success: false, message: "No branch selected" };
      }
      await rpc.gitCheckout(ctx.repoId, input);
      return { success: true, message: `Checked out ${input}` };
    },
  },
  {
    id: "git.branch.create",
    label: "Git: Create Branch…",
    category: "Branch",
    prompt: { kind: "ref", placeholder: "New branch name…" },
    isEnabled: () => true,
    execute: async (ctx: CommandContext, input?: string): Promise<CommandResult> => {
      if (!input) {
        return { success: false, message: "Branch name is required" };
      }
      await rpc.gitBranchCreate(ctx.repoId, input);
      return { success: true, message: `Created branch ${input}` };
    },
  },
  {
    id: "git.branch.delete",
    label: "Git: Delete Branch…",
    category: "Branch",
    prompt: { kind: "branch", placeholder: "Branch to delete…" },
    isEnabled: () => true,
    execute: async (ctx: CommandContext, input?: string): Promise<CommandResult> => {
      if (!input) {
        return { success: false, message: "No branch selected" };
      }
      if (input === ctx.currentBranch) {
        return { success: false, message: "Cannot delete the currently checked-out branch" };
      }
      await rpc.gitBranchDelete(ctx.repoId, input);
      return { success: true, message: `Deleted branch ${input}` };
    },
  },
  {
    id: "git.branch.merge",
    label: "Git: Merge Branch…",
    category: "Branch",
    prompt: { kind: "branch", placeholder: "Branch to merge…" },
    isEnabled: () => true,
    execute: async (ctx: CommandContext, input?: string): Promise<CommandResult> => {
      if (!input) {
        return { success: false, message: "No branch selected" };
      }
      await rpc.gitMerge(ctx.repoId, input);
      return { success: true, message: `Merged ${input} into ${ctx.currentBranch}` };
    },
  },
  {
    id: "git.branch.rename",
    label: "Git: Rename Branch…",
    category: "Branch",
    prompt: { kind: "ref", placeholder: "New name…" },
    isEnabled: (ctx: CommandContext) => ctx.currentBranch.length > 0,
    execute: async (ctx: CommandContext, input?: string): Promise<CommandResult> => {
      if (!input) {
        return { success: false, message: "New branch name is required" };
      }
      if (!ctx.currentBranch) {
        return { success: false, message: "No current branch to rename" };
      }
      await rpc.gitBranchRename(ctx.repoId, ctx.currentBranch, input);
      return { success: true, message: `Renamed ${ctx.currentBranch} to ${input}` };
    },
  },
];


// --- History category ---

const historyCommands: Command[] = [
  {
    id: "git.rebase",
    label: "Git: Rebase…",
    category: "History",
    prompt: { kind: "branch", placeholder: "Rebase onto…" },
    isEnabled: () => true,
    execute: async (ctx: CommandContext, input?: string): Promise<CommandResult> => {
      if (!input) {
        return { success: false, message: "No target branch selected" };
      }
      await rpc.gitRebase(ctx.repoId, input);
      return { success: true, message: `Rebased onto ${input}` };
    },
  },
  {
    id: "git.revert",
    label: "Git: Revert Commit…",
    category: "History",
    prompt: { kind: "commit", placeholder: "Select commit to revert…" },
    isEnabled: () => true,
    execute: async (ctx: CommandContext, input?: string): Promise<CommandResult> => {
      if (!input) {
        return { success: false, message: "No commit selected" };
      }
      await rpc.gitRevert(ctx.repoId, input);
      return { success: true, message: `Reverted commit ${input.slice(0, 7)}` };
    },
  },
  {
    id: "git.cherry-pick",
    label: "Git: Cherry-Pick Commit…",
    category: "History",
    prompt: { kind: "commit", placeholder: "Select commit…" },
    isEnabled: () => true,
    execute: async (ctx: CommandContext, input?: string): Promise<CommandResult> => {
      if (!input) {
        return { success: false, message: "No commit selected" };
      }
      await rpc.gitCherryPick(ctx.repoId, input);
      return { success: true, message: `Cherry-picked commit ${input.slice(0, 7)}` };
    },
  },
  {
    id: "git.reset",
    label: "Git: Reset…",
    category: "History",
    prompt: {
      kind: "choice",
      placeholder: "Reset mode…",
      choices: [
        { label: "Soft", value: "soft" },
        { label: "Mixed", value: "mixed" },
        { label: "Hard", value: "hard" },
      ],
      next: { kind: "commit", placeholder: "Reset to…" },
    },
    destructive: true,
    isEnabled: () => true,
    execute: async (ctx: CommandContext, input?: string): Promise<CommandResult> => {
      if (!input) {
        return { success: false, message: "No target specified" };
      }
      // Input format for multi-step prompts: "mode:target"
      // The palette joins multi-step inputs with ":"
      const separatorIdx = input.indexOf(":");
      let mode: "soft" | "mixed" | "hard" = "mixed";
      let target = input;
      if (separatorIdx !== -1) {
        const modeStr = input.slice(0, separatorIdx);
        target = input.slice(separatorIdx + 1);
        if (modeStr === "soft" || modeStr === "mixed" || modeStr === "hard") {
          mode = modeStr;
        }
      }
      await rpc.gitReset(ctx.repoId, target, { mode });
      return { success: true, message: `Reset (${mode}) to ${target.slice(0, 7)}` };
    },
  },
];


// --- Stash category ---

const stashCommands: Command[] = [
  {
    id: "git.stash",
    label: "Git: Stash",
    category: "Stash",
    isEnabled: () => true,
    execute: async (ctx: CommandContext, input?: string): Promise<CommandResult> => {
      await rpc.gitStashSave(ctx.repoId, { message: input || undefined });
      return { success: true, message: input ? `Stashed: ${input}` : "Changes stashed" };
    },
  },
  {
    id: "git.stash.untracked",
    label: "Git: Stash (Include Untracked)",
    category: "Stash",
    isEnabled: () => true,
    execute: async (ctx: CommandContext, input?: string): Promise<CommandResult> => {
      await rpc.gitStashSave(ctx.repoId, { message: input || undefined, include_untracked: true });
      return { success: true, message: "Changes stashed (including untracked)" };
    },
  },
  {
    id: "git.stash.pop",
    label: "Git: Pop Stash",
    category: "Stash",
    isEnabled: () => true,
    execute: async (ctx: CommandContext, input?: string): Promise<CommandResult> => {
      const ref = input || "stash@{0}";
      await rpc.gitStashPop(ctx.repoId, ref);
      return { success: true, message: "Stash popped" };
    },
  },
  {
    id: "git.stash.apply",
    label: "Git: Apply Stash",
    category: "Stash",
    isEnabled: () => true,
    execute: async (ctx: CommandContext, input?: string): Promise<CommandResult> => {
      const ref = input || "stash@{0}";
      await rpc.gitStashApply(ctx.repoId, ref);
      return { success: true, message: "Stash applied" };
    },
  },
  {
    id: "git.stash.drop",
    label: "Git: Drop Stash",
    category: "Stash",
    destructive: true,
    isEnabled: () => true,
    execute: async (ctx: CommandContext, input?: string): Promise<CommandResult> => {
      if (!input) {
        return { success: false, message: "Stash reference is required" };
      }
      await rpc.gitStashDrop(ctx.repoId, input);
      return { success: true, message: `Dropped ${input}` };
    },
  },
];


// --- Stage category ---

const stageCommands: Command[] = [
  {
    id: "git.stage.all",
    label: "Git: Stage All",
    category: "Stage",
    isEnabled: (ctx: CommandContext) => ctx.dirtyCount > 0,
    execute: async (ctx: CommandContext): Promise<CommandResult> => {
      // Stage all: use `git add -A` which handles tracked modifications,
      // untracked files, and deletions in one shot.
      await rpc.gitAddPaths(ctx.repoId, ["."]);
      return { success: true, message: "All changes staged" };
    },
  },
  {
    id: "git.unstage.all",
    label: "Git: Unstage All",
    category: "Stage",
    isEnabled: (ctx: CommandContext) => ctx.hasStagedChanges,
    execute: async (ctx: CommandContext): Promise<CommandResult> => {
      // Unstage all: use `git reset HEAD` which handles all staged files.
      await rpc.gitResetPaths(ctx.repoId, ["."]);
      return { success: true, message: "All changes unstaged" };
    },
  },
  {
    id: "git.discard.all",
    label: "Git: Discard All Changes",
    category: "Stage",
    destructive: true,
    isEnabled: (ctx: CommandContext) => ctx.dirtyCount > 0,
    execute: async (ctx: CommandContext): Promise<CommandResult> => {
      // Reset working tree to HEAD
      await rpc.gitCheckout(ctx.repoId, ".", { pathspec: true });
      return { success: true, message: "All changes discarded" };
    },
  },
];

// --- Register all commands at module load time ---

const ALL_COMMANDS: Command[] = [
  ...commitCommands,
  ...syncCommands,
  ...branchCommands,
  ...historyCommands,
  ...stashCommands,
  ...stageCommands,
];

for (const cmd of ALL_COMMANDS) {
  registerCommand(cmd);
}
