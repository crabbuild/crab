import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { mainProcess, rpc } from "../ipc";
import { useActiveRepo } from "../state";
import { DEFAULT_SETTINGS, useSettings, type Settings } from "../settings";

const WORKSPACE_SETTINGS_REL = ".crab/desktop/settings.json";

export interface UseWorkspaceSettings {
  workspaceSettings: Partial<Settings> | null;
  loading: boolean;
  hasRepo: boolean;
  setWorkspaceSetting: <K extends keyof Settings>(key: K, value: Settings[K]) => Promise<void>;
  clearWorkspaceSetting: (key: keyof Settings) => Promise<void>;
  getEffective: <K extends keyof Settings>(key: K) => Settings[K];
}

/**
 * Reads and manages per-workspace settings stored in
 * `<repo>/.crab/desktop/settings.json`. Returns the current
 * workspace overrides (partial — only fields that are overridden),
 * plus helpers to set/clear individual keys.
 *
 * Settings precedence: user < workspace < session overrides.
 * The effective value for any setting is:
 *   `workspaceSettings[key] ?? userSettings[key]`
 *
 * Committable: users can check in the workspace settings file to
 * share ignore policies / hydration presets with teammates.
 */
export function useWorkspaceSettings(): UseWorkspaceSettings {
  const activeRepo = useActiveRepo();
  const { settings: userSettings } = useSettings();
  const [workspaceSettings, setWorkspaceSettings] = useState<Partial<Settings> | null>(null);
  const [loading, setLoading] = useState(false);

  // Keep a ref to the latest workspace settings so async callbacks
  // always see the current value without stale closures.
  const settingsRef = useRef<Partial<Settings> | null>(null);
  settingsRef.current = workspaceSettings;

  // Load workspace settings when the active repo changes.
  useEffect(() => {
    if (!activeRepo || activeRepo.deferred) {
      setWorkspaceSettings(null);
      return;
    }

    let cancelled = false;
    setLoading(true);

    readWorkspaceSettings(activeRepo.repo_id)
      .then((settings) => {
        if (cancelled) return;
        setWorkspaceSettings(settings);
      })
      .finally(() => {
        if (!cancelled) setLoading(false);
      });

    return () => {
      cancelled = true;
    };
  }, [activeRepo?.repo_id]);

  const setWorkspaceSetting = useCallback(
    async <K extends keyof Settings>(key: K, value: Settings[K]) => {
      if (!activeRepo?.path) return;
      const existing = await readWorkspaceSettings(activeRepo.repo_id);
      const updated: Partial<Settings> = { ...existing, [key]: value };
      await writeWorkspaceSettings(activeRepo.path, updated);
      setWorkspaceSettings(updated);
    },
    [activeRepo],
  );

  const clearWorkspaceSetting = useCallback(
    async (key: keyof Settings) => {
      if (!activeRepo?.path) return;
      const existing = await readWorkspaceSettings(activeRepo.repo_id);
      if (!existing) {
        setWorkspaceSettings(null);
        return;
      }
      const { [key]: _removed, ...rest } = existing;
      const updated = Object.keys(rest).length > 0 ? (rest as Partial<Settings>) : null;
      if (updated) {
        await writeWorkspaceSettings(activeRepo.path, updated);
      } else {
        // Write an empty object rather than deleting the file — keeps
        // the file present so git tracks it if the user committed it.
        await writeWorkspaceSettings(activeRepo.path, {});
      }
      setWorkspaceSettings(updated);
    },
    [activeRepo],
  );

  const getEffective = useCallback(
    <K extends keyof Settings>(key: K): Settings[K] => {
      const ws = settingsRef.current;
      if (ws && key in ws) {
        return ws[key] as Settings[K];
      }
      return userSettings[key] ?? DEFAULT_SETTINGS[key];
    },
    [userSettings],
  );

  const hasRepo = activeRepo !== null && !!activeRepo.path;

  return useMemo<UseWorkspaceSettings>(
    () => ({
      workspaceSettings,
      loading,
      hasRepo,
      setWorkspaceSetting,
      clearWorkspaceSetting,
      getEffective,
    }),
    [workspaceSettings, loading, hasRepo, setWorkspaceSetting, clearWorkspaceSetting, getEffective],
  );
}

// ── Internal helpers ────────────────────────────────────────────────

async function readWorkspaceSettings(
  repoId: string,
): Promise<Partial<Settings> | null> {
  try {
    const res = await rpc.readText(repoId, WORKSPACE_SETTINGS_REL, {
      max_bytes: 64 * 1024,
    });
    if (!res.text) return null;
    const parsed = JSON.parse(res.text);
    if (!parsed || typeof parsed !== "object") return null;
    return parsed as Partial<Settings>;
  } catch {
    // File doesn't exist or isn't readable — no workspace override.
    return null;
  }
}

async function writeWorkspaceSettings(
  repoPath: string,
  settings: Partial<Settings>,
): Promise<void> {
  const content = JSON.stringify(settings, null, 2) + "\n";
  const fullPath = `${repoPath}/${WORKSPACE_SETTINGS_REL}`;
  await mainProcess.fsWriteFile(fullPath, content);
}
