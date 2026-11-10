import { useCallback, useEffect, useState } from "react";
import type { DetectedAgent } from "../../../electron/orchestrator-adapters";

/** Keep availability and late replies bound to the selected repository host. */
export function useAgentAvailability(repoId: string) {
  type Catalog = { repoId: string; agents: DetectedAgent[]; ready: boolean | null; error?: string };
  const [catalog, setCatalog] = useState<Catalog>({ repoId, agents: [], ready: null });
  const [revision, setRevision] = useState(0);
  const retry = useCallback(() => setRevision(value => value + 1), []);
  const ready = useCallback((agents: DetectedAgent[]) => setCatalog({ repoId, agents, ready: true }), [repoId]);
  const skip = useCallback(() => setCatalog(current => ({ repoId, agents: current.repoId === repoId ? current.agents : [], ready: true })), [repoId]);
  useEffect(() => {
    let active = true;
    let refreshed = false;
    setCatalog({ repoId, agents: [], ready: null });
    const apply = (result: Awaited<ReturnType<Window["crab"]["orchestrator"]["detectAgents"]>>, fresh: boolean) => {
      if (!active || (!fresh && (refreshed || !result.cached))) return;
      refreshed ||= fresh;
      if (!result.success) {
        setCatalog({ repoId, agents: [], ready: false, error: result.error ?? "Agent detection failed" });
        return;
      }
      const agents = result.agents ?? [];
      setCatalog({ repoId, agents, ready: agents.some(agent => agent.available) ? true : fresh ? false : null });
    };
    if (!repoId.startsWith("ssh://")) {
      void window.crab.orchestrator.detectAgents({ repoId, cachedOnly: true }).then(result => apply(result, false)).catch(() => {});
    }
    void window.crab.orchestrator.detectAgents({ repoId, force: revision > 0 }).then(result => apply(result, true)).catch((error: unknown) => {
      if (active) setCatalog({ repoId, agents: [], ready: false, error: error instanceof Error ? error.message : String(error) });
    });
    const profileId = repoId.startsWith("ssh://") ? repoId.slice(6).split("/")[0] : undefined;
    const unsubscribe = profileId ? window.crab.ssh.onStatusChanged(event => {
      if (event.profileId !== profileId) return;
      active = false;
      const connected = event.status === "connected";
      setCatalog({ repoId, agents: [], ready: connected ? null : false, error: connected ? undefined : event.error ?? `SSH connection ${event.status}` });
      if (event.status === "connected") retry();
    }) : undefined;
    return () => { active = false; unsubscribe?.(); };
  }, [repoId, revision, retry]);
  const current = catalog.repoId === repoId ? catalog : { agents: [], ready: null, error: undefined };
  return { agents: current.agents, setupComplete: current.ready, error: current.error, retry, ready, skip };
}
