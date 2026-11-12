import { Button } from "@/components/ui/button";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuGroup,
  DropdownMenuItem,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import clsx from "clsx";
import { Activity, ArrowLeft, ArrowRight, ChevronDown, ChevronRight, Cloud, FolderGit2, FolderTree, PanelLeftOpen, Terminal } from "lucide-react";
import { useEffect, useRef, useState } from "react";
import { type StartupCommand } from "../lib/terminal-presets";
import { OpenRepo, WorkspaceRepoEntry, useStore } from "../state";
import { TerminalPresetMenu } from "./PresetDropdown";

interface Props {
  /** Whether the outer Repositories sidebar is visible. */
  repoSidebarOpen: boolean;
  /** Called when the user clicks the title-bar "expand repositories" button. */
  onExpandRepoSidebar: () => void;
  /** Currently-focused repo, surfaced in the chrome so the main
   *  pane recovers the vertical space. `null` when no repo is open. */
  activeRepo: OpenRepo | null;
  /** Navigation history controls. */
  canBack: boolean;
  canForward: boolean;
  onBack: () => void;
  onForward: () => void;
  /** Open the full-page terminal. */
  terminalActive: boolean;
  onOpenTerminal: () => void;
  onShowOverview: () => void;
  activityDrawerOpen: boolean;
  onToggleActivityDrawer: () => void;
  /** Called when user selects an agent preset from the dropdown. */
  onSpawnAgentTerminal?: (cmd: StartupCommand) => void;
  /** Workspace name (when a workspace is active). */
  workspaceName?: string | null;
  /** All repos in the active workspace (for the dropdown switcher). */
  workspaceRepos?: WorkspaceRepoEntry[];
  /** Currently active workspace repo relative path. */
  activeWorkspaceRelativePath?: string | null;
  /** Called when user selects a repo from the workspace dropdown. */
  onWorkspaceRepoSelect?: (relativePath: string) => void;
}

/**
 * Top chrome (title-bar replacement on macOS).
 *
 * Electron's `titleBarStyle: "hiddenInset"` hides the native titlebar
 * but still renders the traffic lights in the top-left. For the
 * window to remain draggable we designate a chrome region ourselves
 * via `-webkit-app-region: drag`; interactive elements inside that
 * region need `-webkit-app-region: no-drag` or clicks fall through
 * to the OS window manager.
 *
 * macOS traffic lights occupy ~78 px from the window's left edge,
 * so we pad the content to keep the logo clear of them.
 *
 * The centre region now carries the active repo's label + URL so
 * we can drop the per-pane header in `<RepoDetail/>`. Cache dir /
 * remote indicator live in Settings → Workspace → Cache (less
 * valuable for the at-a-glance view).
 */
export function TopBar({
  repoSidebarOpen,
  onExpandRepoSidebar,
  activeRepo,
  canBack,
  canForward,
  onBack,
  onForward,
  terminalActive,
  onOpenTerminal,
  onShowOverview,
  activityDrawerOpen,
  onToggleActivityDrawer,
  onSpawnAgentTerminal,
  workspaceName,
  workspaceRepos,
  activeWorkspaceRelativePath,
  onWorkspaceRepoSelect,
}: Props) {
  const { state } = useStore();
  const running = Object.values(state.jobs).filter(job => job.status === "running").length;
  // Derive worktree display info from the active repo label.
  const isWorktree = activeRepo?.label.startsWith("↳") ?? false;
  const worktreeName = isWorktree
    ? activeRepo!.label.replace(/^↳\s*(?:worktree:|wt:)?/, "")
    : null;

  // Wrap onSpawnAgentTerminal to inject agentMeta derived from the active
  // workspace and the selected agent preset before passing it downstream.
  const handleSpawnAgentTerminalWithMeta = onSpawnAgentTerminal
    ? (cmd: StartupCommand) => {
        const worktreePath = activeRepo?.path ?? activeRepo?.cache_dir ?? "";
        const agentType = cmd.icon ?? cmd.id;
        const taskId = `topbar-${agentType}-${Date.now()}`;
        onSpawnAgentTerminal({
          ...cmd,
          agentMeta: { taskId, agentType, worktreePath },
        });
      }
    : undefined;

  // Workspace repo dropdown state.
  const [wsDropdownOpen, setWsDropdownOpen] = useState(false);
  const wsDropdownRef = useRef<HTMLDivElement>(null);

  // Close workspace dropdown on outside click.

  // Update the window title to reflect the active repo/worktree context.
  useEffect(() => {
    if (!activeRepo) {
      document.title = "Crab Desktop";
      return;
    }
    if (isWorktree && worktreeName) {
      // Extract parent repo name from the URL or path for context.
      const parentName = activeRepo.url
        ? (activeRepo.url.split("/").filter(Boolean).pop() ?? "repo")
        : "repo";
      document.title = `Crab Desktop — ${parentName} / ${worktreeName}`;
    } else {
      document.title = `Crab Desktop — ${activeRepo.label}`;
    }
  }, [activeRepo, isWorktree, worktreeName]);

  return (
    <header
      className="flex h-11 shrink-0 items-center gap-3 border-b border-border bg-card pr-3 pl-[84px]"
      style={{ WebkitAppRegion: "drag" } as React.CSSProperties}
    >
      <div
        className="flex shrink-0 items-center gap-1"
        style={{ WebkitAppRegion: "no-drag" } as React.CSSProperties}
      >
        {!repoSidebarOpen && (
          <Button
            variant="ghost"
            size="compact"
            type="button"
            className="flex h-7 w-7 items-center justify-center rounded text-muted-foreground transition hover:bg-accent hover:text-foreground"
            onClick={onExpandRepoSidebar}
            title="Show Repositories (⇧⌘B)"
            aria-label="Show Repositories"
          >
            <PanelLeftOpen size={14} />
          </Button>
        )}
        <Button
          variant="ghost"
          size="compact"
          type="button"
          className="flex h-7 w-7 items-center justify-center rounded text-muted-foreground transition hover:bg-accent hover:text-foreground disabled:opacity-30 disabled:pointer-events-none"
          onClick={onBack}
          disabled={!canBack}
          title="Go Back (⌘[)"
          aria-label="Go back"
        >
          <ArrowLeft size={14} />
        </Button>
        <Button
          variant="ghost"
          size="compact"
          type="button"
          className="flex h-7 w-7 items-center justify-center rounded text-muted-foreground transition hover:bg-accent hover:text-foreground disabled:opacity-30 disabled:pointer-events-none"
          onClick={onForward}
          disabled={!canForward}
          title="Go Forward (⌘])"
          aria-label="Go forward"
        >
          <ArrowRight size={14} />
        </Button>
      </div>

      {/* Dropdown list of workspace repos */}
      <div className="flex min-w-0 flex-1 items-center justify-center">
        {workspaceName && workspaceRepos && workspaceRepos.length > 0 && activeWorkspaceRelativePath && onWorkspaceRepoSelect ? (
          /* Workspace mode: active repo belongs to a workspace — show workspace icon + name + repo dropdown */
          <div
            className="relative"
            ref={wsDropdownRef}
            style={{ WebkitAppRegion: "no-drag" } as React.CSSProperties}
          >
            <DropdownMenu
              open={wsDropdownOpen}
              onOpenChange={setWsDropdownOpen}
            >
              <div className="flex max-w-full items-center rounded-md border border-border/60 bg-background text-xs">
                <Button
                  variant="ghost"
                  size="compact"
                  type="button"
                  aria-label="Repository overview"
                  title="Repository overview"
                  onClick={onShowOverview}
                  className="flex min-w-0 items-center gap-2 rounded-l-md px-3 py-1 hover:bg-accent"
                >
                  <FolderGit2 size={12} className="shrink-0 text-primary" />
                  <span className="max-w-32 truncate text-muted-foreground">
                    {workspaceName}
                  </span>
                  {activeRepo && (
                    <>
                      <ChevronRight size={10} className="shrink-0 text-muted-foreground" />
                      <span className="max-w-40 truncate text-foreground">
                        {activeRepo.label}
                      </span>
                    </>
                  )}
                </Button>
                <DropdownMenuTrigger asChild>
                  <Button
                    variant="ghost"
                    size="compact"
                    type="button"
                    aria-label="Switch repository"
                    aria-expanded={wsDropdownOpen}
                    onClick={() => setWsDropdownOpen((value) => !value)}
                    className="rounded-r-md border-l border-border px-2 py-2 text-muted-foreground hover:bg-accent"
                  >
                    <ChevronDown size={10} />
                  </Button>
                </DropdownMenuTrigger>
              </div>

              {/* Main button — opens the terminal page */}
              {wsDropdownOpen && (
                <DropdownMenuContent
                  side="bottom"
                  align="start"
                  className="w-64 max-h-[320px] overflow-y-auto rounded-lg border border-border bg-card py-1"
                >
                  <DropdownMenuGroup>
                    {workspaceRepos.map((entry) => {
                      const isActive = entry.relativePath === activeWorkspaceRelativePath;
                      const isDirty = (entry.dirtyCount ?? 0) > 0;
                      return (
                        <DropdownMenuItem
                          key={entry.relativePath}
                          className={clsx(
                        "flex w-full items-center gap-2 px-3 py-1.5 text-left text-[11px] transition",
                        isActive
                          ? "bg-primary/10 text-foreground"
                          : "text-muted-foreground hover:bg-accent hover:text-foreground",
                      )}
                          onSelect={() => {
                        setWsDropdownOpen(false);
                        onWorkspaceRepoSelect(entry.relativePath);
                      }}
                        >
                          <span className="min-w-0 flex-1 truncate font-medium">
                            {entry.name}
                          </span>
                          {entry.branch && (
                            <span className="shrink-0 text-[10px] text-muted-foreground font-mono max-w-[80px] truncate">
                              {entry.branch}
                            </span>
                          )}
                          {isDirty && (
                        <span className="shrink-0 h-2 w-2 rounded-full bg-warning" title={`${entry.dirtyCount} changes`} />
                      )}
                          {isActive && (
                        <span className="shrink-0 h-1.5 w-1.5 rounded-full bg-primary" />
                      )}
                        </DropdownMenuItem>
                      );
                    })}
                    {workspaceRepos.length === 0 && (
                      <div className="px-3 py-2 text-[11px] text-muted-foreground text-center">
                        No repos in workspace
                      </div>
                    )}
                  </DropdownMenuGroup>
                </DropdownMenuContent>
              )}
            </DropdownMenu>
          </div>
        ) : activeRepo ? (
          /* Single repo mode: standalone repo — show repo icon + name, no dropdown */
          <Button
            variant="ghost"
            size="compact"
            className="flex min-w-0 max-w-full items-center gap-2 rounded-md border border-border/60 bg-background px-3 py-1 text-[12px]"
            style={{ WebkitAppRegion: "no-drag" } as React.CSSProperties}
            title={`${activeRepo.label}\n${activeRepo.url}`}
            type="button"
            aria-label="Repository overview"
            onClick={onShowOverview}
          >
            {isWorktree ? (
              <FolderTree size={12} className="shrink-0 text-primary" />
            ) : activeRepo.is_remote ? (
              <Cloud size={12} className="shrink-0 text-primary" />
            ) : (
              <FolderGit2 size={12} className="shrink-0 text-muted-foreground" />
            )}
            {isWorktree && worktreeName ? (
              <>
                <span className="shrink-0 text-muted-foreground text-[11px]">
                  {activeRepo.url
                    ? (activeRepo.url.split("/").filter(Boolean).pop() ?? "repo")
                    : "repo"}
                </span>
                <ChevronRight size={10} className="shrink-0 text-muted-foreground/60" />
                <span className="min-w-0 truncate font-medium text-primary">
                  {worktreeName}
                </span>
              </>
            ) : (
              <span className="min-w-0 truncate font-medium text-foreground">
                {activeRepo.label}
              </span>
            )}
          </Button>
        ) : (
          <div
            className="select-none text-[11px] text-muted-foreground"
            style={{ WebkitAppRegion: "no-drag" } as React.CSSProperties}
          >
            No repository open
          </div>
        )}
      </div>

      <div
        className="flex shrink-0 items-center gap-2"
        style={{ WebkitAppRegion: "no-drag" } as React.CSSProperties}
      >
        <Button
          variant="ghost"
          size="compact"
          type="button"
          onClick={onToggleActivityDrawer}
          className={clsx(
            "flex h-7 items-center gap-1.5 rounded px-2 text-[11px] text-muted-foreground transition hover:bg-accent hover:text-foreground",
            activityDrawerOpen && "bg-primary/15 text-primary",
            running > 0 && !activityDrawerOpen && "text-warning",
          )}
          title="Toggle Activity (background jobs)"
          aria-label="Activity"
          aria-expanded={activityDrawerOpen}
          aria-controls="activity-drawer"
        >
          <Activity size={14} />
          Activity
          {running > 0 && (
            <span className="rounded bg-warning/20 px-1 tabular-nums text-warning">
              {running}
            </span>
          )}
        </Button>
        <TerminalSplitButton
          terminalActive={terminalActive}
          onOpenTerminal={onOpenTerminal}
          onSpawnAgentTerminal={handleSpawnAgentTerminalWithMeta}
        />
      </div>
    </header>
  );
}

// --- Terminal Split Button with Agent Dropdown ---

interface TerminalSplitButtonProps {
  terminalActive: boolean;
  onOpenTerminal: () => void;
  onSpawnAgentTerminal?: (cmd: StartupCommand) => void;
}

function TerminalSplitButton({
  terminalActive,
  onOpenTerminal,
  onSpawnAgentTerminal,
}: TerminalSplitButtonProps) {
  return (
    <div className="inline-flex">
      <Button
        variant="outline"
        className={clsx(
          "rounded-r-none border-r-0 text-xs",
          terminalActive && "border-primary text-primary",
        )}
        onClick={onOpenTerminal}
        title="Open Terminal (⌃`)"
      >
        <Terminal size={14} />
        Terminal
      </Button>
      <TerminalPresetMenu
        onSpawnPlain={onOpenTerminal}
        onSelect={(command) => onSpawnAgentTerminal?.(command)}
      >
        <Button
          variant="outline"
          className={clsx(
            "rounded-l-none border-l-0 px-1.5",
            terminalActive && "border-primary text-primary",
          )}
          aria-label="Terminal options"
        >
          <ChevronDown size={10} />
        </Button>
      </TerminalPresetMenu>
    </div>
  );
}
