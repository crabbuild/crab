import { Button } from "@/components/ui/button";
import { Badge } from "@/components/ui/badge";
import {
  Empty,
  EmptyHeader,
  EmptyMedia,
  EmptyTitle,
  EmptyDescription,
  EmptyContent,
} from "@/components/ui/empty";
import {
  Popover,
  PopoverContent,
  PopoverTrigger,
} from "@/components/ui/popover";
import {
  Check,
  ChevronDown,
  Download,
  FolderSearch,
  Loader2,
  Plus,
  Puzzle,
  RefreshCw,
} from "lucide-react";
import { useEffect, useState } from "react";
import { SkillCreateModal } from "../components/skills/SkillCreateModal";
import { SkillDetail } from "../components/skills/SkillDetail";
import { SkillInstallModal } from "../components/skills/SkillInstallModal";
import { SkillList } from "../components/skills/SkillList";
import { AgentIcon } from "../components/orchestrator/AgentIcon";
import { publishToast } from "../lib/toasts";
import { SkillsProvider, useSkills } from "../state/skills-store";

export function SkillsView({ repoPath }: { repoPath?: string }) {
  return (
    <SkillsProvider repoPath={repoPath}>
      <SkillsViewInner />
    </SkillsProvider>
  );
}

function AgentFolders() {
  const { state, refreshAgents } = useSkills();
  const found = state.detectedAgents.filter((agent) => agent.installed).length;
  return (
    <Popover>
      <PopoverTrigger asChild>
        <Button variant="ghost" size="sm">
          <FolderSearch aria-hidden="true" />
          {state.agentsLoading
            ? "Scanning agents…"
            : state.agentsError
              ? "Agent scan failed"
              : `Agent folders · ${found}/${state.detectedAgents.length}`}
          <ChevronDown aria-hidden="true" className="size-3" />
        </Button>
      </PopoverTrigger>
      <PopoverContent
        align="end"
        className="w-96 max-w-[calc(100vw-2rem)] space-y-3"
      >
        <div>
          <h3 className="text-sm font-semibold">Agent folders</h3>
          <p className="mt-1 text-xs text-muted-foreground">
            Crab checks local configuration folders. A found folder does not
            confirm the CLI is installed or signed in.
          </p>
        </div>
        {state.agentsError && (
          <p role="alert" className="text-xs text-destructive">
            {state.agentsError}
          </p>
        )}
        <ul className="max-h-80 overflow-y-auto divide-y divide-border">
          {state.detectedAgents.map((agent) => (
            <li key={agent.id} className="flex items-start gap-2 py-2">
              <span aria-hidden="true" className="mt-0.5">
                <AgentIcon agentName={agent.id} size={16} />
              </span>
              <div className="min-w-0 flex-1">
                <p className="text-xs font-medium">{agent.name}</p>
                <p className="mt-0.5 break-all font-mono text-[11px] text-muted-foreground">
                  {agent.configDir}
                </p>
              </div>
              <Badge variant="secondary" className="shrink-0 gap-1 text-[10px]">
                {agent.installed && (
                  <Check aria-hidden="true" className="size-3" />
                )}
                {agent.installed ? "Found" : "Not found"}
              </Badge>
            </li>
          ))}
        </ul>
        <Button
          variant="outline"
          size="sm"
          onClick={refreshAgents}
          disabled={state.agentsLoading}
        >
          <RefreshCw
            aria-hidden="true"
            className={state.agentsLoading ? "animate-spin" : ""}
          />{" "}
          Scan again
        </Button>
      </PopoverContent>
    </Popover>
  );
}

function SkillsViewInner() {
  const { state, resyncAll, refreshSkills } = useSkills();
  const [showCreate, setShowCreate] = useState(false);
  const [showInstall, setShowInstall] = useState(false);
  const [syncing, setSyncing] = useState(false);

  async function handleSync() {
    setSyncing(true);
    try {
      const issues = await resyncAll();
      publishToast(
        issues
          ? {
              kind: "error",
              title: "Some links need attention",
              body: "Open a skill’s Agent links to see which folders could not be linked. Existing folders are preserved.",
            }
          : {
              kind: "success",
              title: "Skills synced",
              body: "Links are up to date in the agent folders found on this computer.",
            },
      );
    } catch (e) {
      publishToast({
        kind: "error",
        title: "Sync failed",
        body: e instanceof Error ? e.message : String(e),
      });
    } finally {
      setSyncing(false);
    }
  }

  useEffect(() => {
    const handleKeyDown = (event: KeyboardEvent) => {
      if (
        (event.metaKey || event.ctrlKey) &&
        event.key === "n" &&
        !event.defaultPrevented &&
        !(
          event.target instanceof HTMLElement &&
          event.target.closest(
            "input, textarea, [contenteditable=true], [role=dialog]",
          )
        )
      ) {
        event.preventDefault();
        setShowCreate(true);
      }
    };
    window.addEventListener("keydown", handleKeyDown);
    return () => window.removeEventListener("keydown", handleKeyDown);
  }, []);

  const actions = (
    <>
      <Button variant="default" size="sm" onClick={() => setShowCreate(true)}>
        <Plus aria-hidden="true" /> New skill
      </Button>
      <Button variant="outline" size="sm" onClick={() => setShowInstall(true)}>
        <Download aria-hidden="true" /> Install from GitHub
      </Button>
    </>
  );

  return (
    <div className="flex h-full min-w-0 flex-col text-foreground">
      <header className="flex shrink-0 flex-wrap items-center gap-2 border-b border-border px-3 py-2">
        <Puzzle aria-hidden="true" className="size-4" />
        <h2 className="text-sm font-semibold">Skills</h2>
        <Badge variant="secondary">{state.skills.length}</Badge>
        <div className="ml-auto flex flex-wrap items-center gap-1">
          <AgentFolders />
          {state.skills.length > 0 && (
            <Button
              variant="ghost"
              size="sm"
              onClick={handleSync}
              disabled={
                syncing ||
                state.loading ||
                !state.detectedAgents.some((agent) => agent.installed)
              }
            >
              <RefreshCw
                aria-hidden="true"
                className={syncing ? "animate-spin" : ""}
              />{" "}
              {syncing ? "Syncing…" : "Sync skills"}
            </Button>
          )}
          {state.skills.length > 0 && actions}
        </div>
      </header>
      {state.error ? (
        <Empty role="alert">
          <EmptyHeader>
            <EmptyTitle>Could not load skills</EmptyTitle>
            <EmptyDescription>{state.error}</EmptyDescription>
          </EmptyHeader>
          <Button size="sm" onClick={refreshSkills} disabled={state.loading}>
            Retry loading skills
          </Button>
        </Empty>
      ) : state.loading && state.skills.length === 0 ? (
        <div
          role="status"
          className="flex flex-1 items-center justify-center gap-2 text-sm text-muted-foreground"
        >
          <Loader2 aria-hidden="true" className="size-4 animate-spin" /> Loading
          skills…
        </div>
      ) : state.skills.length === 0 ? (
        <Empty>
          <EmptyHeader>
            <EmptyMedia variant="icon">
              <Puzzle aria-hidden="true" />
            </EmptyMedia>
            <EmptyTitle>Build your skills library</EmptyTitle>
            <EmptyDescription>
              Save reusable instructions for your agents. Create a skill or
              install an existing SKILL.md from GitHub.
            </EmptyDescription>
          </EmptyHeader>
          <EmptyContent>
            <div className="flex flex-wrap justify-center gap-2">{actions}</div>
            <p className="text-xs text-muted-foreground">
              Crab links library skills to agent folders found on this computer.
              Skills managed outside Crab are not listed here.
            </p>
          </EmptyContent>
        </Empty>
      ) : (
        <div className="flex min-h-0 flex-1">
          <SkillList />
          <SkillDetail key={state.selectedSkillId} />
        </div>
      )}
      <SkillCreateModal
        open={showCreate}
        onClose={() => setShowCreate(false)}
      />
      <SkillInstallModal
        open={showInstall}
        onClose={() => setShowInstall(false)}
      />
    </div>
  );
}
