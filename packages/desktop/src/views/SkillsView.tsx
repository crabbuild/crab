import { useState, useEffect, useCallback } from "react";
import { Puzzle, Plus, Download, RefreshCw } from "lucide-react";
import { SkillsProvider, useSkills } from "../state/skills-store";
import { SkillList } from "../components/skills/SkillList";
import { SkillDetail } from "../components/skills/SkillDetail";
import { SkillCreateModal } from "../components/skills/SkillCreateModal";
import { SkillInstallModal } from "../components/skills/SkillInstallModal";

interface SkillsViewProps {
  repoPath?: string;
}

export function SkillsView({ repoPath }: SkillsViewProps) {
  return (
    <SkillsProvider repoPath={repoPath}>
      <SkillsViewInner />
    </SkillsProvider>
  );
}

function AgentStatusDots() {
  const { state } = useSkills();
  const agents = state.detectedAgents;

  return (
    <div className="flex items-center gap-1" title={agents.map((a) => `${a.name}: ${a.installed ? "installed" : "not found"}`).join("\n")}>
      {agents.map((a) => (
        <span
          key={a.id}
          className={`inline-block h-2 w-2 rounded-full ${a.installed ? "bg-success" : "bg-warning"}`}
        />
      ))}
    </div>
  );
}

function SkillsViewInner() {
  const { state, resyncAll, refreshSkills } = useSkills();
  const [showCreate, setShowCreate] = useState(false);
  const [showInstall, setShowInstall] = useState(false);
  const [syncing, setSyncing] = useState(false);

  const handleResync = async () => {
    setSyncing(true);
    await resyncAll();
    setSyncing(false);
  };

  const handleRetry = useCallback(() => {
    refreshSkills();
  }, [refreshSkills]);

  // Cmd+N / Ctrl+N to open create modal
  useEffect(() => {
    const handleKeyDown = (e: KeyboardEvent) => {
      if ((e.metaKey || e.ctrlKey) && e.key === "n") {
        e.preventDefault();
        setShowCreate(true);
      }
    };
    window.addEventListener("keydown", handleKeyDown);
    return () => window.removeEventListener("keydown", handleKeyDown);
  }, []);

  return (
    <div className="flex flex-col h-full">
      {/* Toolbar */}
      <div className="flex items-center gap-2 px-3 py-2 border-b border-border shrink-0">
        <Puzzle size={18} className="text-primary" />
        <h2 className="text-sm font-semibold text-foreground">Skills</h2>
        <div className="flex-1" />
        <AgentStatusDots />
        <button
          className="p-1.5 rounded hover:bg-accent text-muted-foreground hover:text-foreground transition-colors"
          onClick={handleResync}
          disabled={syncing}
          title="Resync skills to all agents"
        >
          <RefreshCw size={14} className={syncing ? "animate-spin" : ""} />
        </button>
        <button
          className="flex items-center gap-1 rounded-md bg-primary px-3 py-1.5 text-xs font-medium text-primary-foreground hover:opacity-90 transition-opacity"
          onClick={() => setShowCreate(true)}
        >
          <Plus size={14} />
          New Skill
        </button>
        <button
          className="flex items-center gap-1 rounded-md border border-border px-3 py-1.5 text-xs font-medium text-foreground hover:bg-accent transition-colors"
          onClick={() => setShowInstall(true)}
        >
          <Download size={14} />
          Install from URL
        </button>
      </div>

      {/* Content */}
      {state.loading ? (
        <div className="flex-1 flex items-center justify-center text-muted-foreground text-sm">
          Loading skills...
        </div>
      ) : state.error ? (
        <div className="flex-1 flex flex-col items-center justify-center gap-3">
          <p className="text-destructive text-sm">{state.error}</p>
          <button
            className="rounded-md border border-border px-3 py-1.5 text-xs font-medium text-foreground hover:bg-accent transition-colors"
            onClick={handleRetry}
          >
            Retry
          </button>
        </div>
      ) : (
        <div className="flex flex-1 min-h-0">
          <SkillList />
          <SkillDetail />
        </div>
      )}

      {/* Modals */}
      <SkillCreateModal open={showCreate} onClose={() => setShowCreate(false)} />
      <SkillInstallModal open={showInstall} onClose={() => setShowInstall(false)} />
    </div>
  );
}
