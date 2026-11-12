import {
  Select,
  SelectContent,
  SelectGroup,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { AlertCircle, GitBranch } from "lucide-react";
import { useEffect, useMemo } from "react";
import { getActiveWorkspace, useStore, WorkspaceRepoEntry } from "../../state";

interface Props {
  /** Currently selected repo relative path. */
  selectedRepoPath: string | null;
  /** Called when the user picks a different repo. */
  onSelect: (relativePath: string) => void;
}

/**
 * Dropdown selector listing all workspace repos with branch and dirty
 * count. Used in the agent task UI to require explicit repo selection
 * before the agent begins work.
 *
 * Only renders when a workspace is active. The selected repo's path is
 * passed to the orchestrator's createTask call so the agent is
 * constrained to that single repo.
 *
 * When only a single repo exists in the workspace, auto-selects it and
 * renders a read-only display instead of a dropdown to reduce friction.
 */
export function WorkspaceRepoSelector({ selectedRepoPath, onSelect }: Props) {
  const { state } = useStore();
  const workspace = getActiveWorkspace(state);

  const repos: WorkspaceRepoEntry[] = useMemo(() => {
    if (!workspace) return [];
    return [...workspace.repos].sort((a, b) =>
      a.name.localeCompare(b.name, undefined, { sensitivity: "base" }),
    );
  }, [workspace]);

  // Auto-select when there's exactly one repo and nothing is selected yet.
  useEffect(() => {
    if (repos.length === 1 && !selectedRepoPath) {
      onSelect(repos[0].relativePath);
    }
  }, [repos, selectedRepoPath, onSelect]);

  if (!workspace) return null;

  // No repos discovered yet — don't render the selector at all.
  // The dialog will skip the repo requirement when repos are empty.
  if (repos.length === 0) return null;

  // Single repo: show a compact read-only display instead of a dropdown.
  if (repos.length === 1) {
    const repo = repos[0];
    return (
      <div>
        <label className="mb-1 block text-[11px] font-medium text-muted-foreground">
          Target repository
        </label>
        <div className="flex items-center gap-2 rounded-md border border-border bg-background px-3 py-1.5 text-xs text-foreground">
          <span className="truncate font-medium">{repo.name}</span>
          {repo.branch && (
            <span className="flex items-center gap-1 text-muted-foreground">
              <GitBranch size={10} />
              {repo.branch}
            </span>
          )}
          {repo.dirtyCount != null && repo.dirtyCount > 0 && (
            <span className="text-warning">{repo.dirtyCount} uncommitted</span>
          )}
          {repo.error && (
            <span className="flex items-center gap-1 text-destructive">
              <AlertCircle size={10} />
              {repo.error}
            </span>
          )}
        </div>
      </div>
    );
  }

  return (
    <div>
      <label className="mb-1 block text-[11px] font-medium text-muted-foreground">
        Target repository
      </label>
      <div className="relative">
        <Select
          value={":" + String(selectedRepoPath ?? "")}
          onValueChange={(selectedValue) => {
            const value = selectedValue.slice(1);
            return onSelect(value);
          }}
        >
          <SelectTrigger
            className="w-full appearance-none rounded-md border border-border bg-background px-3 py-1.5 pr-8 text-xs text-foreground focus:border-primary focus:outline-none"
            aria-label="Select target repository"
          >
            <SelectValue />
          </SelectTrigger>
          <SelectContent>
            <SelectGroup>
              <SelectItem disabled value={":" + String("")}>
                Select a repository…
              </SelectItem>
              {repos.map((repo) => (
                <SelectItem
                  key={repo.relativePath}
                  value={":" + String(repo.relativePath)}
                >
                  {repo.name}
                  {repo.branch ? ` (${repo.branch})` : ""}
                  {repo.dirtyCount && repo.dirtyCount > 0
                ? ` • ${repo.dirtyCount} changed`
                : ""}
                  {repo.error ? " ⚠" : ""}
                </SelectItem>
              ))}
            </SelectGroup>
          </SelectContent>
        </Select>
        {/* Chevron indicator */}
        <div className="pointer-events-none absolute inset-y-0 right-2 flex items-center text-muted-foreground">
          <svg width="10" height="10" viewBox="0 0 10 10" fill="currentColor">
            <path d="M2 3.5L5 6.5L8 3.5" stroke="currentColor" strokeWidth="1.2" fill="none" />
          </svg>
        </div>
      </div>

      {/* Status detail for selected repo */}
      {selectedRepoPath && (
        <SelectedRepoStatus repos={repos} selectedPath={selectedRepoPath} />
      )}
    </div>
  );
}

function SelectedRepoStatus({
  repos,
  selectedPath,
}: {
  repos: WorkspaceRepoEntry[];
  selectedPath: string;
}) {
  const entry = repos.find((r) => r.relativePath === selectedPath);
  if (!entry) return null;

  return (
    <div className="mt-1.5 flex items-center gap-3 text-[10px] text-muted-foreground">
      {entry.branch && (
        <span className="flex items-center gap-1">
          <GitBranch size={10} />
          {entry.branch}
        </span>
      )}
      {entry.dirtyCount != null && entry.dirtyCount > 0 && (
        <span className="text-warning">{entry.dirtyCount} uncommitted</span>
      )}
      {entry.error && (
        <span className="flex items-center gap-1 text-destructive">
          <AlertCircle size={10} />
          {entry.error}
        </span>
      )}
    </div>
  );
}
