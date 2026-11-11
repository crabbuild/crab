import { useCallback } from "react";
import { useWorkspaceDocument } from "../settings";
import type { Settings } from "../settings";
import { isWorkspaceSetting, workspaceOverrides } from "../lib/workspace-settings-store";

/** Expose acknowledged workspace overrides from the shared document owner. */
export function useWorkspaceSettings() {
  const state = useWorkspaceDocument();
  const setWorkspaceSetting = useCallback(<K extends keyof Settings>(key: K, value: Settings[K]) => state.change(document => {
    if (!isWorkspaceSetting(key)) throw new Error("This setting belongs to the device. Choose User settings.");
    document[key] = value;
  }), [state.change]);
  const clearWorkspaceSetting = useCallback((key: keyof Settings) => state.change(document => { delete document[key]; }), [state.change]);
  return {
    workspaceSettings: state.document ? workspaceOverrides(state.document) : null,
    loading: state.loading, saving: state.saving, error: state.error, hasRepo: state.canEdit,
    reload: state.reload, setWorkspaceSetting, clearWorkspaceSetting,
  };
}
