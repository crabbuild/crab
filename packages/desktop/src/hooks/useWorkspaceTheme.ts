import { useCallback, useEffect, useState } from "react";
import { mainProcess, rpc } from "../ipc";
import { useActiveRepo } from "../state";
import type { Settings } from "../settings";

export type ThemeValue = Settings["theme"];

interface WorkspaceSettings {
  theme?: ThemeValue;
  [key: string]: unknown;
}

const WORKSPACE_SETTINGS_REL = ".crab/desktop/settings.json";

/**
 * Reads and manages the per-workspace theme override stored in
 * `<repo>/.crab/desktop/settings.json`. Returns the current
 * workspace theme (or null if none), plus helpers to set/clear it.
 *
 * The workspace theme takes precedence over the user-level setting
 * when determining which CSS class to apply on `<html>`.
 */
export function useWorkspaceTheme() {
  const activeRepo = useActiveRepo();
  const [workspaceTheme, setWorkspaceTheme] = useState<ThemeValue | null>(null);
  const [loading, setLoading] = useState(false);

  // Load workspace settings when the active repo changes.
  useEffect(() => {
    if (!activeRepo || activeRepo.deferred) {
      setWorkspaceTheme(null);
      return;
    }

    let cancelled = false;
    setLoading(true);

    readWorkspaceSettings(activeRepo.repo_id)
      .then((settings) => {
        if (cancelled) return;
        setWorkspaceTheme(settings?.theme ?? null);
      })
      .finally(() => {
        if (!cancelled) setLoading(false);
      });

    return () => {
      cancelled = true;
    };
  }, [activeRepo?.repo_id]);

  const setForWorkspace = useCallback(
    async (theme: ThemeValue) => {
      if (!activeRepo?.path) return;
      const existing = await readWorkspaceSettings(activeRepo.repo_id);
      const updated: WorkspaceSettings = { ...existing, theme };
      await writeWorkspaceSettings(activeRepo.path, updated);
      setWorkspaceTheme(theme);
    },
    [activeRepo],
  );

  const clearWorkspaceOverride = useCallback(async () => {
    if (!activeRepo?.path) return;
    const existing = await readWorkspaceSettings(activeRepo.repo_id);
    if (!existing) {
      setWorkspaceTheme(null);
      return;
    }
    const { theme: _removed, ...rest } = existing;
    await writeWorkspaceSettings(activeRepo.path, rest as WorkspaceSettings);
    setWorkspaceTheme(null);
  }, [activeRepo]);

  return {
    workspaceTheme,
    loading,
    hasRepo: activeRepo !== null && !!activeRepo.path,
    setForWorkspace,
    clearWorkspaceOverride,
  };
}

async function readWorkspaceSettings(
  repoId: string,
): Promise<WorkspaceSettings | null> {
  try {
    const res = await rpc.readText(repoId, WORKSPACE_SETTINGS_REL, {
      max_bytes: 64 * 1024,
    });
    if (!res.text) return null;
    const parsed = JSON.parse(res.text);
    if (!parsed || typeof parsed !== "object") return null;
    return parsed as WorkspaceSettings;
  } catch {
    // File doesn't exist or isn't readable — no workspace override.
    return null;
  }
}

async function writeWorkspaceSettings(
  repoPath: string,
  settings: WorkspaceSettings,
): Promise<void> {
  const content = JSON.stringify(settings, null, 2) + "\n";
  const fullPath = `${repoPath}/${WORKSPACE_SETTINGS_REL}`;
  await mainProcess.fsWriteFile(fullPath, content);
}
