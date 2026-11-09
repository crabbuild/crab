/**
 * Core commands registered at module load time.
 *
 * These seed the registry with navigation and git actions that the
 * command palette, keybindings, and context menus can resolve.
 */

import { commands } from "./index";
import { rpc } from "../ipc";
import { publishToast } from "../lib/toasts";

// --- View commands ---

commands.register({
  id: "view.explorer",
  title: "Navigate to Explorer",
  category: "View",
  keywords: ["files", "tree", "sidebar"],
  run: async () => {
    // Dispatched via the app's navigation system once wired.
  },
});

commands.register({
  id: "view.changes",
  title: "Navigate to Changes",
  category: "View",
  keywords: ["diff", "staged", "unstaged", "modified"],
  run: async () => {
    // Dispatched via the app's navigation system once wired.
  },
});

commands.register({
  id: "view.timeline",
  title: "Navigate to Timeline",
  category: "View",
  keywords: ["log", "history", "commits"],
  run: async () => {
    // Dispatched via the app's navigation system once wired.
  },
});

commands.register({
  id: "view.branches",
  title: "Navigate to Branches",
  category: "View",
  keywords: ["branch", "checkout", "switch"],
  run: async () => {
    // Dispatched via the app's navigation system once wired.
  },
});

commands.register({
  id: "view.settings",
  title: "Open Settings",
  category: "View",
  keywords: ["preferences", "config", "options", "diagnostics", "health"],
  run: async () => {
    // Dispatched via the app's navigation system once wired.
  },
});

// --- Git commands ---

commands.register({
  id: "git.commit",
  title: "Commit staged changes",
  category: "Git",
  keywords: ["save", "snapshot"],
  run: async () => {
    // Wired to the commit composer flow once integrated.
  },
});

commands.register({
  id: "git.push",
  title: "Push current branch",
  category: "Git",
  keywords: ["upload", "remote", "origin"],
  run: async () => {
    // Wired to the push action once integrated.
  },
});

commands.register({
  id: "git.pull",
  title: "Pull current branch",
  category: "Git",
  keywords: ["download", "fetch", "merge", "rebase"],
  run: async () => {
    // Wired to the pull action once integrated.
  },
});

commands.register({
  id: "git.fetch",
  title: "Fetch all remotes",
  category: "Git",
  keywords: ["download", "remote", "update"],
  run: async () => {
    // Wired to the fetch-all action once integrated.
  },
});

// --- Additional view commands ---

commands.register({
  id: "view.pull-requests",
  title: "Navigate to Pull Requests",
  category: "View",
  keywords: ["pr", "merge request", "review"],
  run: async () => {},
});

commands.register({
  id: "view.datasets",
  title: "Navigate to Datasets",
  category: "View",
  keywords: ["data", "parquet", "safetensors", "files"],
  run: async () => {},
});

commands.register({
  id: "view.experiments",
  title: "Navigate to Experiments",
  category: "View",
  keywords: ["mlflow", "wandb", "runs", "metrics"],
  run: async () => {},
});

commands.register({
  id: "view.terminal",
  title: "Navigate to Terminal",
  category: "View",
  keywords: ["shell", "console", "cli"],
  run: async () => {},
});

commands.register({
  id: "view.conflicts",
  title: "Navigate to Conflicts",
  category: "View",
  keywords: ["merge", "resolve", "conflict"],
  run: async () => {},
});

commands.register({
  id: "view.stash",
  title: "Navigate to Changes & Stashes",
  category: "View",
  keywords: ["stash", "saved", "shelved", "changes"],
  run: async () => {},
});

commands.register({
  id: "view.pointers",
  title: "Navigate to Pointer Map",
  category: "View",
  keywords: ["pointer", "xorb", "dehydrated", "lazy"],
  run: async () => {},
});

commands.register({
  id: "view.dashboard",
  title: "Navigate to Dashboard",
  category: "View",
  keywords: ["home", "overview", "summary"],
  run: async () => {},
});

// --- Additional git commands ---

commands.register({
  id: "git.stage-all",
  title: "Stage all changes",
  category: "Git",
  keywords: ["add", "stage", "all"],
  run: async () => {},
});

commands.register({
  id: "git.unstage-all",
  title: "Unstage all changes",
  category: "Git",
  keywords: ["reset", "unstage", "all"],
  run: async () => {},
});

commands.register({
  id: "git.discard-all",
  title: "Discard all changes",
  category: "Git",
  keywords: ["checkout", "restore", "discard", "revert"],
  run: async () => {},
});

commands.register({
  id: "git.stash",
  title: "Stash changes",
  category: "Git",
  keywords: ["stash", "save", "shelve"],
  run: async () => {},
});

commands.register({
  id: "git.stash-pop",
  title: "Pop latest stash",
  category: "Git",
  keywords: ["stash", "pop", "restore"],
  run: async () => {},
});

commands.register({
  id: "git.merge",
  title: "Merge branch",
  category: "Git",
  keywords: ["merge", "combine", "integrate"],
  run: async () => {},
});

commands.register({
  id: "git.rebase",
  title: "Rebase branch",
  category: "Git",
  keywords: ["rebase", "replay", "linearize"],
  run: async () => {},
});

commands.register({
  id: "git.cherry-pick",
  title: "Cherry-pick commit",
  category: "Git",
  keywords: ["cherry", "pick", "apply"],
  run: async () => {},
});

commands.register({
  id: "git.branch-create",
  title: "Create new branch",
  category: "Git",
  keywords: ["branch", "new", "create"],
  run: async () => {},
});

commands.register({
  id: "git.branch-delete",
  title: "Delete branch",
  category: "Git",
  keywords: ["branch", "delete", "remove"],
  run: async () => {},
});

commands.register({
  id: "git.checkout",
  title: "Checkout branch",
  category: "Git",
  keywords: ["checkout", "switch", "branch"],
  run: async () => {},
});

// --- Crab commands ---

commands.register({
  id: "crab.hydrate",
  title: "Hydrate files",
  category: "Crab",
  keywords: ["hydrate", "download", "materialize", "restore"],
  run: async () => {},
});

commands.register({
  id: "crab.dehydrate",
  title: "Dehydrate files",
  category: "Crab",
  keywords: ["dehydrate", "pointer", "shrink", "lazy"],
  run: async () => {},
});

commands.register({
  id: "crab.add",
  title: "Track files with Crab",
  category: "Crab",
  keywords: ["add", "track", "large", "pointer"],
  run: async () => {},
});

// --- Forge commands ---

commands.register({
  id: "forge.create-pr",
  title: "Create pull request",
  category: "Forge",
  keywords: ["pr", "pull request", "merge request", "create"],
  run: async () => {},
});

commands.register({
  id: "forge.list-prs",
  title: "List pull requests",
  category: "Forge",
  keywords: ["pr", "pull request", "merge request", "list"],
  run: async () => {},
});

// --- App commands ---

commands.register({
  id: "app.open-logs",
  title: "Open logs folder",
  category: "App",
  keywords: ["logs", "debug", "folder"],
  run: async () => {},
});

commands.register({
  id: "app.copy-diagnostics",
  title: "Copy diagnostics to clipboard",
  category: "App",
  keywords: ["diagnostics", "copy", "clipboard", "debug"],
  run: async () => {},
});

commands.register({
  id: "app.toggle-devtools",
  title: "Toggle developer tools",
  category: "App",
  keywords: ["devtools", "inspector", "debug", "console"],
  run: async () => {},
});

commands.register({
  id: "app.command-palette",
  title: "Open command palette",
  category: "App",
  keywords: ["palette", "commands", "search", "actions"],
  run: async () => {
    // Dispatches a custom event that the Shell component listens for
    // to open the palette. This allows the palette to be opened from
    // within itself or from any other command/keybinding.
    window.dispatchEvent(new CustomEvent("crab:open-command-palette"));
  },
});

commands.register({
  id: "app.open-sample-repo",
  title: "Open sample repository",
  category: "App",
  keywords: ["sample", "demo", "tutorial", "example", "onboarding"],
  run: async () => {
    const SAMPLE_REPO_URL = "crab://crab-samples/demo-repo";
    const SAMPLE_REPO_DEST = "~/Crab/sample-repo";

    try {
      // Try opening the destination directly — if it already exists
      // from a previous clone, skip the clone step entirely.
      const handle = await rpc.openLocal(SAMPLE_REPO_DEST);
      window.dispatchEvent(
        new CustomEvent("crab:repo-opened", { detail: { handle, path: SAMPLE_REPO_DEST } }),
      );
    } catch {
      // Destination doesn't exist or isn't a valid repo — clone it.
      try {
        await rpc.clone(SAMPLE_REPO_URL, SAMPLE_REPO_DEST);
        const handle = await rpc.openLocal(SAMPLE_REPO_DEST);
        window.dispatchEvent(
          new CustomEvent("crab:repo-opened", { detail: { handle, path: SAMPLE_REPO_DEST } }),
        );
        publishToast({
          kind: "success",
          title: "Sample repository ready",
          body: "The demo repo has been cloned and opened.",
        });
      } catch (err) {
        publishToast({
          kind: "error",
          title: "Failed to open sample repository",
          body: err instanceof Error ? err.message : String(err),
        });
      }
    }
  },
});

// --- File context commands ---

commands.register({
  id: "file.time-machine",
  title: "Git Time Machine",
  category: "File",
  keywords: ["time", "machine", "history", "blame", "slider", "revision"],
  run: async () => {
    // The command dispatches a custom event with the file path.
    // The RepoDetail component listens for this event and opens
    // the TimeMachineView for the specified file.
    // When invoked from the context menu, the path is set before
    // dispatching. When invoked from the palette without context,
    // the currently active file is used (handled by the listener).
    window.dispatchEvent(new CustomEvent("crab:open-time-machine"));
  },
});
