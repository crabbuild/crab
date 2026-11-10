import { useCallback, useEffect, useRef, useState } from "react";
import type { AgentModel } from "../../../electron/orchestrator-adapters";

/** Load the catalog from the configured provider in this repository's context. */
export function useAgentModels(agentType: string | undefined, cwd: string | null | undefined) {
  const [catalog, setCatalog] = useState<{ key: string; models: AgentModel[]; status: string | null; loading: boolean }>({ key: "", models: [], status: null, loading: false });
  const [revision, setRevision] = useState(0);
  const active = useRef<{ key: string; id: string } | null>(null);
  const key = `${agentType ?? ""}\0${cwd ?? ""}`;
  const cancel = useCallback(() => {
    const request = active.current;
    if (!request || request.key !== key) return;
    active.current = null;
    void window.crab.orchestrator.cancelModelDiscovery(request.id).catch(() => {});
    setCatalog({ key, models: [], status: "Model discovery cancelled", loading: false });
  }, [key]);
  const retry = useCallback(() => setRevision(value => value + 1), []);
  useEffect(() => {
    if (!agentType || !cwd) { setCatalog({ key, models: [], status: null, loading: false }); return; }
    const id = crypto.randomUUID();
    active.current = { key, id };
    setCatalog({ key, models: [], status: "Loading models…", loading: true });
    window.crab.orchestrator.listModels(agentType, cwd, id).then(result => {
      if (active.current?.id !== id) return;
      active.current = null;
      setCatalog({ key, models: result.success ? result.models ?? [] : [], status: result.success ? null : `Could not load models: ${result.error ?? "provider unavailable"}`, loading: false });
    }).catch((error: unknown) => {
      if (active.current?.id !== id) return;
      active.current = null;
      setCatalog({ key, models: [], status: `Could not load models: ${error instanceof Error ? error.message : "provider unavailable"}`, loading: false });
    });
    return () => {
      if (active.current?.id !== id) return;
      active.current = null;
      void window.crab.orchestrator.cancelModelDiscovery(id).catch(() => {});
    };
  }, [agentType, cwd, key, revision]);
  const current = catalog.key === key ? catalog : { models: [], status: null, loading: false };
  return { models: current.models, status: current.status, loading: current.loading, cancel, retry };
}

/** Keep an explicitly configured model visible even when absent from the picker. */
export function modelOptions(models: AgentModel[], currentModel?: string | null): AgentModel[] {
  if (!currentModel || models.some(model => model.value === currentModel)) return models;
  return [...models, { value: currentModel, label: currentModel, description: "Configured model", intelligenceLevels: [] }];
}

export function intelligenceLabel(level: string): string {
  if (level === "xhigh" || level === "extra-high") return "Extra High";
  return level.charAt(0).toUpperCase() + level.slice(1);
}
