import { Button } from "@/components/ui/button";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuGroup,
  DropdownMenuItem,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
/**
 * Agent conversations sharing a workspace, with independent status and chat access.
 */

import clsx from "clsx";
import { Activity, Bot, GitMerge, Loader2, Plus } from "lucide-react";
import { useCallback, useState } from "react";
import type { AgentSessionInfo } from "../../lib/use-agent-sessions";
import { AgentIcon, agentAccentBg, agentAccentColor } from "./AgentIcon";
import { OrchestratorTask } from "./TaskCard";

// ── Types ───────────────────────────────────────────────────────────

interface Props {
  task: OrchestratorTask;
  childTasks: OrchestratorTask[];
  onOpenChat: (taskId: string) => void;
  onReviewChanges: () => void;
  /** Callback to add a new agent to the task. */
  onAddAgent?: (agentType: string, role: string) => void;
  /** Live daemon session states for this task's agents. */
  daemonSessions?: AgentSessionInfo[];
}

// ── Preset roles for quick-add ──────────────────────────────────────

const PRESET_ROLES = [
  { label: "Backend", role: "backend engineer", description: "API, database, server logic" },
  { label: "Frontend", role: "frontend engineer", description: "UI, components, styling" },
  { label: "Tests", role: "test engineer", description: "Unit tests, integration tests" },
  { label: "Reviewer", role: "code reviewer", description: "Review and suggest improvements" },
];

// ── Component ───────────────────────────────────────────────────────

export function MultiAgentView({ task, childTasks, onOpenChat, onReviewChanges, onAddAgent, daemonSessions = [] }: Props) {
  const [showAddMenu, setShowAddMenu] = useState(false);
  const conversations = [task, ...childTasks];

  const handleAddAgent = useCallback((role: string) => {
    onAddAgent?.(task.agentName, role);
    setShowAddMenu(false);
  }, [task.agentName, onAddAgent]);

  const allDone = conversations.every(conversation => ["reviewing", "applied", "archived", "discarded", "failed", "stopped"].includes(conversation.status));
  const hasMultiple = conversations.length > 1;

  return (
    <div className="flex h-full flex-col">
      {/* Header */}
      <div className="flex items-center gap-2 border-b border-border px-4 py-2">
        <Bot size={13} className="text-primary" />
        <span className="text-xs font-medium text-foreground">Agents</span>
        <span className="text-[10px] text-muted-foreground">
          ({conversations.length})
        </span>
        <div className="flex-1" />
        <div className="relative">
          <DropdownMenu open={showAddMenu} onOpenChange={setShowAddMenu}>
            <DropdownMenuTrigger asChild>
              <Button
                variant="ghost"
                size="compact"
                type="button"
                onClick={() => setShowAddMenu(!showAddMenu)}
                className="flex items-center gap-1 rounded px-2 py-1 text-[10px] text-foreground hover:bg-accent"
              >
                <Plus size={10} /> Add agent
              </Button>
            </DropdownMenuTrigger>

            {showAddMenu && (
              <DropdownMenuContent
                side="bottom"
                align="end"
                className=" w-[200px] rounded-md border border-border bg-card "
              >
                <DropdownMenuGroup>
                  {PRESET_ROLES.map((preset) => (
                    <DropdownMenuItem
                      key={preset.role}
                      onSelect={() => handleAddAgent(preset.role)}
                      className="justify-start flex w-full flex-col px-3 py-2 text-left hover:bg-accent"
                    >
                      <span className="text-[11px] font-medium text-foreground">
                        {preset.label}
                      </span>
                      <span className="text-[9px] text-muted-foreground">
                        {preset.description}
                      </span>
                    </DropdownMenuItem>
                  ))}
                </DropdownMenuGroup>
              </DropdownMenuContent>
            )}
          </DropdownMenu>
        </div>
      </div>

      {/* Agent lanes (Gantt-like) */}
      <div className="flex-1 overflow-auto px-4 py-3">
        <div className="space-y-2">
          {conversations.map((conversation) => (
            <AgentLaneRow key={conversation.id} task={conversation} primary={conversation.id === task.id} onOpenChat={onOpenChat} />
          ))}
        </div>

        {/* Daemon session states — live activity from the PTY daemon */}
        {daemonSessions.length > 0 && (
          <div className="mt-4">
            <div className="mb-2 flex items-center gap-1.5">
              <Activity size={11} className="text-primary" />
              <span className="text-[10px] font-medium text-muted-foreground uppercase tracking-wide">
                Daemon Sessions
              </span>
            </div>
            <div className="space-y-1.5">
              {daemonSessions.map((session) => (
                <DaemonSessionRow key={session.sessionId} session={session} />
              ))}
            </div>
          </div>
        )}

        {/* Merge prompt when all agents are done */}
        {hasMultiple && allDone && (
          <div className="mt-4 rounded-md border border-primary/30 bg-primary/5 p-3">
            <div className="flex items-center gap-2">
              <GitMerge size={14} className="text-primary" />
              <span className="text-xs font-medium text-foreground">
                All agents finished
              </span>
            </div>
            <p className="mt-1 text-[10px] text-muted-foreground">
              Multiple agents worked on this task. Their changes are in the same
              worktree. Review the diff to check for conflicts.
            </p>
            <Button
              variant="default"
              size="compact"
              type="button"
              onClick={onReviewChanges}
              className="mt-2 flex items-center gap-1.5 rounded bg-primary px-3 py-1.5 text-[11px] font-medium text-primary-foreground hover:bg-primary/80 disabled:opacity-50"
            >
              <GitMerge size={11} />
              Review changes
            </Button>
          </div>
        )}
      </div>
    </div>
  );
}

// ── Lane Row ────────────────────────────────────────────────────────

function AgentLaneRow({ task, primary, onOpenChat }: { task: OrchestratorTask; primary: boolean; onOpenChat: (taskId: string) => void }) {
  const running = task.status === "running" || task.status === "iterating";
  return (
    <div className="flex items-center gap-3 rounded-md border border-border bg-background px-3 py-2">
      <div className={clsx(
        "h-2.5 w-2.5 shrink-0 rounded-full",
        running ? "bg-success animate-pulse" : task.status === "failed" ? "bg-destructive" : "bg-muted-foreground/40",
      )} />

      {/* Provider icon */}
      <div
        className={clsx(
          "flex h-6 w-6 shrink-0 items-center justify-center rounded-full",
          agentAccentBg(task.agentName),
        )}
        title={task.agentName}
      >
        <AgentIcon agentName={task.agentName} size={12} className={agentAccentColor(task.agentName)} />
      </div>

      {/* Agent info */}
      <div className="flex-1 min-w-0">
        <div className="flex items-center gap-2">
          <span className="text-[11px] font-medium text-foreground capitalize">
            {primary ? "primary" : (task.spawnedRole ?? task.title)}
          </span>
          <span className="text-[9px] text-muted-foreground">
            {task.agentName}
          </span>
        </div>
      </div>

      <div className="flex shrink-0 items-center gap-1 text-[11px] capitalize text-muted-foreground" role="status">
        {running && <Loader2 size={10} className="animate-spin" />}
        {task.status}
      </div>
      <Button
        variant="ghost"
        size="compact"
        type="button"
        onClick={() => onOpenChat(task.id)}
        aria-label={`Open ${primary ? "primary" : (task.spawnedRole ?? task.title)} chat`}
        className="shrink-0 rounded px-2 py-1 text-xs text-primary hover:bg-accent focus-visible:outline focus-visible:outline-2 focus-visible:outline-primary"
      >
        Open chat
      </Button>
    </div>
  );
}

// ── Daemon Session Row ──────────────────────────────────────────────

/** Formats elapsed milliseconds as a compact human-readable string. */
function formatElapsed(ms: number): string {
  if (ms < 1000) return "<1s";
  const totalSeconds = Math.floor(ms / 1000);
  const hours = Math.floor(totalSeconds / 3600);
  const minutes = Math.floor((totalSeconds % 3600) / 60);
  const seconds = totalSeconds % 60;
  if (hours > 0) return `${hours}h ${minutes}m`;
  if (minutes > 0) return `${minutes}m ${seconds}s`;
  return `${seconds}s`;
}

function DaemonSessionRow({ session }: { session: AgentSessionInfo }) {
  const state = session.state as "working" | "waiting" | "idle" | "exited" | string;

  return (
    <div className="flex items-center gap-2.5 rounded border border-border bg-background px-2.5 py-1.5">
      {/* State dot */}
      <div
        className={clsx(
          "h-2 w-2 shrink-0 rounded-full",
          state === "working" && "bg-success animate-pulse",
          state === "waiting" && "bg-warning",
          state === "idle" && "bg-muted-foreground/40",
          state === "exited" && "bg-muted-foreground/20",
        )}
      />

      {/* Agent type + session id */}
      <div className="flex-1 min-w-0">
        <div className="flex items-center gap-1.5">
          <span className="text-[10px] font-medium text-foreground capitalize">
            {session.agentType}
          </span>
          <span className="truncate text-[9px] text-muted-foreground font-mono">
            {session.sessionId.slice(0, 8)}
          </span>
        </div>
      </div>

      {/* State label */}
      <span
        className={clsx(
          "shrink-0 text-[9px] font-medium capitalize",
          state === "working" && "text-success",
          state === "waiting" && "text-warning",
          state === "idle" && "text-muted-foreground",
          state === "exited" && "text-muted-foreground/60",
        )}
      >
        {state}
      </span>

      {/* Elapsed working time — only shown when there has been working time */}
      {session.elapsedWorkingMs > 0 && (
        <span className="shrink-0 text-[9px] text-muted-foreground tabular-nums">
          {formatElapsed(session.elapsedWorkingMs)}
        </span>
      )}
    </div>
  );
}
