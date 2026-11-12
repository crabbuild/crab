import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import clsx from "clsx";
import { GitPullRequestArrow, Layers } from "lucide-react";
import { useCallback, useEffect, useState } from "react";
import { Panel, PanelGroup, PanelResizeHandle } from "react-resizable-panels";
import { SafePatchDiff } from "../components/SafePatchDiff";
import { StashesPanel } from "../components/StashesPanel";
import { getWatchPath, useFileWatcher } from "../hooks/useFileWatcher";
import { rpc } from "../ipc";
import { OpenRepo } from "../state";
import { ChangesView } from "./ChangesView";

type Tab = "changes" | "stashes";

interface Props {
  repo: OpenRepo;
  onOpenDiff?: (path: string) => void;
  activeDiffPath?: string | null;
  onCommit?: (sha: string) => void;
}

const LS_ACTIVE_TAB = "crab.changesStashes.tab";

function loadTab(): Tab {
  const stored = localStorage.getItem(LS_ACTIVE_TAB);
  if (stored === "stashes") return "stashes";
  return "changes";
}

/**
 * Tabbed container that combines the Changes view and Stashes view
 * into a single unified workspace. The Changes tab shows the full
 * existing ChangesView. The Stashes tab shows a stash list on the
 * left with a diff preview pane on the right.
 */
export function ChangesAndStashesView({ repo, onOpenDiff, activeDiffPath, onCommit }: Props) {
  const [activeTab, setActiveTab] = useState<Tab>(loadTab);
  const [stashCount, setStashCount] = useState(0);
  const [dirtyCount, setDirtyCount] = useState(0);

  // Stash diff preview state
  const [selectedStashRef, setSelectedStashRef] = useState<string | null>(null);
  const [selectedFilePath, setSelectedFilePath] = useState<string | null>(null);
  const [stashPatch, setStashPatch] = useState<string | null>(null);
  const [patchLoading, setPatchLoading] = useState(false);

  // Persist tab selection
  useEffect(() => {
    localStorage.setItem(LS_ACTIVE_TAB, activeTab);
  }, [activeTab]);

  // Fetch stash count for the tab badge
  const refreshStashCount = useCallback(() => {
    rpc.gitStashList(repo.repo_id)
      .then((r) => setStashCount(r.stashes.length))
      .catch(() => {});
  }, [repo.repo_id]);

  useEffect(refreshStashCount, [refreshStashCount]);

  // Fetch dirty count for the tab badge
  const refreshDirtyCount = useCallback(() => {
    rpc.gitStatus(repo.repo_id)
      .then((r) => setDirtyCount(r.entries.length))
      .catch(() => {});
  }, [repo.repo_id]);

  useEffect(refreshDirtyCount, [refreshDirtyCount]);

  // Single file-watcher subscription for both badge counts.
  const refreshBadges = useCallback(() => {
    refreshStashCount();
    refreshDirtyCount();
  }, [refreshStashCount, refreshDirtyCount]);
  useFileWatcher(getWatchPath(repo), refreshBadges);

  // ── Stash diff preview ─────────────────────────────────────────────

  const handleSelectStash = useCallback(
    (stashRef: string) => {
      setSelectedStashRef(stashRef);
      setSelectedFilePath(null);
      setPatchLoading(true);
      setStashPatch(null);
      rpc
        .gitStashShowPatch(repo.repo_id, stashRef)
        .then((r) => setStashPatch(r.patch))
        .catch(() => setStashPatch(null))
        .finally(() => setPatchLoading(false));
    },
    [repo.repo_id],
  );

  const handleSelectStashFile = useCallback(
    (stashRef: string, filePath: string) => {
      setSelectedStashRef(stashRef);
      setSelectedFilePath(filePath);
      setPatchLoading(true);
      setStashPatch(null);
      // Get the full stash patch and filter to the specific file
      rpc
        .gitStashShowPatch(repo.repo_id, stashRef)
        .then((r) => {
          // Filter patch to only show the selected file
          const filtered = filterPatchToFile(r.patch, filePath);
          setStashPatch(filtered);
        })
        .catch(() => setStashPatch(null))
        .finally(() => setPatchLoading(false));
    },
    [repo.repo_id],
  );

  return (
    <Tabs
      value={activeTab}
      onValueChange={(value) => setActiveTab(value as Tab)}
      className="flex h-full min-h-0 flex-col">
      {/* Tab bar */}
      <TabsList className="h-auto shrink-0 justify-start rounded-none border-b border-border bg-card p-0">
        <TabButton
          value={"changes"}
          active={activeTab === "changes"}
          icon={<GitPullRequestArrow size={12} />}
          label="Changes"
          badge={dirtyCount > 0 ? dirtyCount : undefined}
        />
        <TabButton
          value={"stashes"}
          active={activeTab === "stashes"}
          icon={<Layers size={12} />}
          label="Stashes"
          badge={stashCount > 0 ? stashCount : undefined}
        />
      </TabsList>

      {/* Tab content */}
      <TabsContent
        value={activeTab}
        className="mt-0 flex min-h-0 flex-1 flex-col"
      >
        {activeTab === "changes" ? (
          <ChangesView
            repo={repo}
            onOpenDiff={onOpenDiff}
            activeDiffPath={activeDiffPath}
            onCommit={onCommit}
          />
        ) : (
          <StashesTabContent
            repo={repo}
            selectedStashRef={selectedStashRef}
            selectedFilePath={selectedFilePath}
            stashPatch={stashPatch}
            patchLoading={patchLoading}
            onSelectStash={handleSelectStash}
            onSelectStashFile={handleSelectStashFile}
          />
        )}
      </TabsContent>
    </Tabs>
  );
}

// ── Tab Button ───────────────────────────────────────────────────────

function TabButton({
  active,
  value,
  icon,
  label,
  badge,
}: {
  active: boolean;
  value: Tab;
  icon: React.ReactNode;
  label: string;
  badge?: number;
}) {
  return (
    <TabsTrigger
      className={clsx(
        "relative flex items-center gap-1.5 px-3 py-2 text-xs font-medium transition",
        active
          ? "text-foreground"
          : "text-muted-foreground hover:text-foreground",
      )}
      value={value}
    >
      {icon}
      {label}
      {badge !== undefined && badge > 0 && (
        <span
          className={clsx(
            "ml-0.5 inline-flex h-4 min-w-[16px] items-center justify-center rounded-full px-1 text-[10px] font-medium",
            active
              ? "bg-primary/20 text-primary"
              : "bg-border text-muted-foreground",
          )}
        >
          {badge > 99 ? "99+" : badge}
        </span>
      )}
      {/* Active indicator */}
      {active && (
        <span className="absolute inset-x-0 bottom-0 h-[2px] rounded-full bg-primary" />
      )}
    </TabsTrigger>
  );
}

// ── Stashes Tab Content (split panel) ────────────────────────────────

function StashesTabContent({
  repo,
  selectedStashRef,
  selectedFilePath,
  stashPatch,
  patchLoading,
  onSelectStash,
  onSelectStashFile,
}: {
  repo: OpenRepo;
  selectedStashRef: string | null;
  selectedFilePath: string | null;
  stashPatch: string | null;
  patchLoading: boolean;
  onSelectStash: (ref: string) => void;
  onSelectStashFile: (ref: string, path: string) => void;
}) {
  return (
    <PanelGroup direction="horizontal" className="h-full">
      {/* Left: stash list */}
      <Panel defaultSize={35} minSize={20} maxSize={60}>
        <StashesPanel
          repoId={repo.repo_id}
          repoPath={getWatchPath(repo)}
          onSelectStash={onSelectStash}
          onSelectStashFile={onSelectStashFile}
          selectedStashRef={selectedStashRef}
          selectedFilePath={selectedFilePath}
        />
      </Panel>

      <PanelResizeHandle className="w-px bg-border hover:bg-primary/40 transition-colors" />

      {/* Right: diff preview */}
      <Panel defaultSize={65} minSize={30}>
        <div className="h-full overflow-auto bg-background">
          {!selectedStashRef && (
            <div className="flex h-full items-center justify-center text-xs text-muted-foreground">
              <div className="text-center">
                <Layers size={20} className="mx-auto mb-2 text-muted-foreground/50" />
                <p>Select a stash to preview its changes</p>
              </div>
            </div>
          )}
          {selectedStashRef && patchLoading && (
            <div className="flex h-full items-center justify-center text-xs text-muted-foreground">
              <span className="flex items-center gap-2">
                <span className="h-3 w-3 animate-spin rounded-full border-2 border-primary border-t-transparent" />
                Loading diff…
              </span>
            </div>
          )}
          {selectedStashRef && !patchLoading && stashPatch && (
            <div className="p-0">
              <SafePatchDiff
                patch={stashPatch}
                className="gx-patch-diff"
              />
            </div>
          )}
          {selectedStashRef && !patchLoading && !stashPatch && (
            <div className="flex h-full items-center justify-center text-xs text-muted-foreground">
              No diff available for this stash
            </div>
          )}
        </div>
      </Panel>
    </PanelGroup>
  );
}

// ── Helpers ──────────────────────────────────────────────────────────

/**
 * Filter a unified patch to only include hunks for a specific file.
 * Looks for `diff --git a/<path> b/<path>` headers and extracts
 * everything until the next `diff --git` header.
 */
function filterPatchToFile(patch: string, filePath: string): string {
  const lines = patch.split("\n");
  const result: string[] = [];
  let capturing = false;

  for (const line of lines) {
    if (line.startsWith("diff --git ")) {
      // Check if this diff section is for our file
      const match = line.match(/diff --git a\/(.+?) b\/(.+)/);
      if (match) {
        const [, aPath, bPath] = match;
        capturing = aPath === filePath || bPath === filePath;
      } else {
        capturing = false;
      }
    }
    if (capturing) {
      result.push(line);
    }
  }

  return result.join("\n");
}
