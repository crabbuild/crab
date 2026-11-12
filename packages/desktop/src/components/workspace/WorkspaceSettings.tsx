import { Button } from "@/components/ui/button";
import { Dialog, DialogContent, DialogTitle } from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import { Plus, RefreshCw, Trash2, X } from "lucide-react";
import { useCallback, useEffect, useRef, useState } from "react";
import type { Workspace, WorkspaceConfig } from "../../state";

export interface WorkspaceSettingsProps {
  workspace: Workspace;
  onClose: () => void;
}

/**
 * Workspace settings panel (modal overlay). Allows users to configure
 * discovery depth, exclude patterns, and re-include manually removed repos.
 */
export function WorkspaceSettings({ workspace, onClose }: WorkspaceSettingsProps) {
  const { config } = workspace;

  // Local editable state
  const [depth, setDepth] = useState(config.discovery.maxDepth);
  const [excludePatterns, setExcludePatterns] = useState<string[]>([
    ...config.discovery.excludePatterns,
  ]);
  const [removedRepos, setRemovedRepos] = useState<string[]>([
    ...config.repos.removed,
  ]);
  const [newPattern, setNewPattern] = useState("");
  const [saving, setSaving] = useState(false);

  const inputRef = useRef<HTMLInputElement>(null);

  // Focus the depth input on mount
  useEffect(() => {
    inputRef.current?.focus();
  }, []);

  // Close on Escape
  useEffect(() => {
    function handleKey(e: KeyboardEvent) {
      if (e.key === "Escape") onClose();
    }
    document.addEventListener("keydown", handleKey);
    return () => document.removeEventListener("keydown", handleKey);
  }, [onClose]);

  const addPattern = useCallback(() => {
    const trimmed = newPattern.trim();
    if (trimmed && !excludePatterns.includes(trimmed)) {
      setExcludePatterns((prev) => [...prev, trimmed]);
      setNewPattern("");
    }
  }, [newPattern, excludePatterns]);

  const removePattern = useCallback((pattern: string) => {
    setExcludePatterns((prev) => prev.filter((p) => p !== pattern));
  }, []);

  const reIncludeRepo = useCallback((repoPath: string) => {
    setRemovedRepos((prev) => prev.filter((p) => p !== repoPath));
  }, []);

  const handleSave = useCallback(async () => {
    setSaving(true);
    try {
      const partialConfig: Partial<WorkspaceConfig> = {
        discovery: {
          maxDepth: depth,
          excludePatterns,
        },
        repos: {
          removed: removedRepos,
        },
      };
      await window.crab.workspace.updateConfig(partialConfig as Record<string, unknown>);
      await window.crab.workspace.refresh();
      onClose();
    } catch {
      // Errors are non-fatal here; the IPC layer handles logging
    } finally {
      setSaving(false);
    }
  }, [depth, excludePatterns, removedRepos, onClose]);

  const handleRescan = useCallback(async () => {
    setSaving(true);
    try {
      const partialConfig: Partial<WorkspaceConfig> = {
        discovery: {
          maxDepth: depth,
          excludePatterns,
        },
        repos: {
          removed: removedRepos,
        },
      };
      await window.crab.workspace.updateConfig(partialConfig as Record<string, unknown>);
      await window.crab.workspace.refresh();
    } catch {
      // Non-fatal
    } finally {
      setSaving(false);
    }
  }, [depth, excludePatterns, removedRepos]);

  return (
    <Dialog
      open
      onOpenChange={(open) => {
        if (!open) onClose();
      }}
    >
      {/* Backdrop */}

      {/* Modal */}
      <DialogContent
        aria-describedby={undefined}
        showCloseButton={false}
        className={
          "w-full max-w-lg rounded-lg border border-border bg-background shadow-2xl" +
          " p-0 gap-0"
        }
      >
        {/* Header */}
        <div className="flex items-center justify-between border-b border-border px-4 py-3">
          <DialogTitle className="text-sm font-semibold text-foreground">
            Workspace Settings
          </DialogTitle>
          <Button
            variant="ghost"
            size="compact"
            type="button"
            className="rounded p-1 text-muted-foreground hover:text-foreground hover:bg-accent transition"
            onClick={onClose}
            aria-label="Close settings"
          >
            <X size={14} />
          </Button>
        </div>

        {/* Body */}
        <div className="max-h-[60vh] overflow-y-auto px-4 py-4 space-y-5">
          {/* Workspace root path (read-only) */}
          <div>
            <label className="block text-xs font-medium text-muted-foreground mb-1">
              Workspace Root
            </label>
            <div className="rounded border border-border bg-background px-3 py-2 text-xs text-foreground/70 select-all">
              {workspace.rootPath}
            </div>
          </div>

          {/* Discovery depth */}
          <div>
            <label
              htmlFor="ws-depth"
              className="block text-xs font-medium text-muted-foreground mb-1"
            >
              Discovery Depth
            </label>
            <Input
              ref={inputRef}
              id="ws-depth"
              type="number"
              min={1}
              max={10}
              value={depth}
              onChange={(e) => setDepth(Math.max(1, Math.min(10, parseInt(e.target.value) || 1)))}
              className="w-20 rounded border border-border bg-background px-2 py-1.5 text-xs text-foreground outline-none focus:border-primary transition"
            />
            <p className="mt-1 text-[10px] text-muted-foreground">
              Maximum folder depth to scan for git repositories (1–10).
            </p>
          </div>

          {/* Exclude patterns */}
          <div>
            <label className="block text-xs font-medium text-muted-foreground mb-1">
              Exclude Patterns
            </label>
            <div className="space-y-1">
              {excludePatterns.map((pattern) => (
                <div
                  key={pattern}
                  className="flex items-center gap-2 rounded border border-border bg-background px-2 py-1.5"
                >
                  <code className="flex-1 text-xs text-foreground">
                    {pattern}
                  </code>
                  <Button
                    variant="ghost"
                    size="compact"
                    type="button"
                    className="shrink-0 rounded p-0.5 text-muted-foreground hover:text-destructive transition"
                    onClick={() => removePattern(pattern)}
                    aria-label={`Remove pattern ${pattern}`}
                  >
                    <Trash2 size={12} />
                  </Button>
                </div>
              ))}
            </div>
            <div className="mt-2 flex items-center gap-2">
              <Input
                type="text"
                value={newPattern}
                onChange={(e) => setNewPattern(e.target.value)}
                onKeyDown={(e) => {
                  if (e.key === "Enter") {
                    e.preventDefault();
                    addPattern();
                  }
                }}
                placeholder="e.g. node_modules"
                className="flex-1 rounded border border-border bg-background px-2 py-1.5 text-xs text-foreground placeholder:text-muted-foreground outline-none focus:border-primary transition"
                aria-label="New exclude pattern"
              />
              <Button
                variant="ghost"
                size="compact"
                type="button"
                className="flex items-center gap-1 rounded border border-border px-2 py-1.5 text-xs text-foreground hover:bg-accent transition"
                onClick={addPattern}
                disabled={!newPattern.trim()}
              >
                <Plus size={12} /> Add
              </Button>
            </div>
          </div>

          {/* Manually removed repos */}
          {removedRepos.length > 0 && (
            <div>
              <label className="block text-xs font-medium text-muted-foreground mb-1">
                Excluded Repositories
              </label>
              <p className="text-[10px] text-muted-foreground mb-2">
                These repos were manually removed from the workspace. Click to
                re-include.
              </p>
              <div className="space-y-1">
                {removedRepos.map((repoPath) => (
                  <div
                    key={repoPath}
                    className="flex items-center gap-2 rounded border border-border bg-background px-2 py-1.5"
                  >
                    <code className="flex-1 text-xs text-foreground truncate">
                      {repoPath}
                    </code>
                    <Button
                      variant="ghost"
                      size="compact"
                      type="button"
                      className="shrink-0 rounded px-2 py-0.5 text-[10px] font-medium text-primary hover:bg-primary/10 transition"
                      onClick={() => reIncludeRepo(repoPath)}
                      aria-label={`Re-include ${repoPath}`}
                    >
                      Re-include
                    </Button>
                  </div>
                ))}
              </div>
            </div>
          )}
        </div>

        {/* Footer */}
        <div className="flex items-center justify-between border-t border-border px-4 py-3">
          <Button
            variant="ghost"
            size="compact"
            type="button"
            className="flex items-center gap-1.5 rounded border border-border px-3 py-1.5 text-xs text-foreground hover:bg-accent transition"
            onClick={handleRescan}
            disabled={saving}
          >
            <RefreshCw size={12} /> Re-scan
          </Button>
          <div className="flex items-center gap-2">
            <Button
              variant="ghost"
              size="compact"
              type="button"
              className="rounded px-3 py-1.5 text-xs text-muted-foreground hover:text-foreground hover:bg-accent transition"
              onClick={onClose}
            >
              Cancel
            </Button>
            <Button
              variant="default"
              size="compact"
              type="button"
              className="rounded bg-primary px-3 py-1.5 text-xs font-medium text-primary-foreground hover:bg-primary/90 transition disabled:opacity-50"
              onClick={handleSave}
              disabled={saving}
            >
              {saving ? "Saving…" : "Save"}
            </Button>
          </div>
        </div>
      </DialogContent>
    </Dialog>
  );
}
