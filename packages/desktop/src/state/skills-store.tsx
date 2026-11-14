import {
  createContext,
  useContext,
  useState,
  useCallback,
  useEffect,
  type ReactNode,
} from "react";
import type {
  SkillListItem,
  SkillDetail,
  DetectedAgent,
  SkillOpResult,
} from "../ipc";

function useSkillsState(repoPath?: string) {
  const [skills, setSkills] = useState<SkillListItem[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [selectedSkillId, selectSkill] = useState<string | null>(null);
  const [selectedSkill, setSelectedSkill] = useState<SkillDetail | null>(null);
  const [detailLoading, setDetailLoading] = useState(false);
  const [detailError, setDetailError] = useState<string | null>(null);
  const [detectedAgents, setDetectedAgents] = useState<DetectedAgent[]>([]);
  const [agentsLoading, setAgentsLoading] = useState(true);
  const [agentsError, setAgentsError] = useState<string | null>(null);
  const [detailRevision, setDetailRevision] = useState(0);

  const refreshSkills = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      const result = await window.crab.skills.listSkills(
        repoPath ? { repoPath } : undefined,
      );
      if (!result.success || !result.data)
        throw new Error(result.error ?? "Could not load skills.");
      const items = result.data;
      setSkills(items);
      selectSkill((id) =>
        id && items.some((item) => item.record.id === id) ? id : null,
      );
      return items;
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
      return null;
    } finally {
      setLoading(false);
    }
  }, [repoPath]);

  const refreshAgents = useCallback(async () => {
    setAgentsLoading(true);
    setAgentsError(null);
    try {
      setDetectedAgents(await window.crab.skills.detectAgents());
    } catch (e) {
      setAgentsError(e instanceof Error ? e.message : String(e));
    } finally {
      setAgentsLoading(false);
    }
  }, []);

  useEffect(() => {
    void refreshSkills();
    void refreshAgents();
  }, [refreshSkills, refreshAgents]);

  useEffect(() => {
    let cancelled = false;
    setSelectedSkill((current) =>
      current?.record.id === selectedSkillId ? current : null,
    );
    setDetailError(null);
    setDetailLoading(Boolean(selectedSkillId));
    if (!selectedSkillId) return;
    // A slower request for the previous selection must never replace this detail.
    void window.crab.skills
      .getSkill(selectedSkillId)
      .then((result) => {
        if (cancelled) return;
        if (result.success && result.data) setSelectedSkill(result.data);
        else setDetailError(result.error ?? "Could not load this skill.");
      })
      .catch((e) => {
        if (!cancelled)
          setDetailError(e instanceof Error ? e.message : String(e));
      })
      .finally(() => {
        if (!cancelled) setDetailLoading(false);
      });
    return () => {
      cancelled = true;
    };
  }, [selectedSkillId, skills, detailRevision]);

  async function mutate<T>(
    operation: () => Promise<SkillOpResult<T>>,
  ): Promise<SkillOpResult<T>> {
    try {
      const result = await operation();
      if (result.success) await refreshSkills();
      return result;
    } catch (e) {
      return {
        success: false,
        error: e instanceof Error ? e.message : String(e),
      };
    }
  }

  async function createSkill(
    data: Parameters<typeof window.crab.skills.createSkill>[0],
  ) {
    const result = await mutate(() =>
      window.crab.skills.createSkill({
        ...data,
        repoPath: data.repoPath ?? repoPath,
      }),
    );
    if (result.success && result.data) selectSkill(result.data.record.id);
    return result;
  }

  async function installFromUrl(url: string) {
    const result = await mutate(() =>
      window.crab.skills.installFromUrl(url, "user", repoPath),
    );
    if (result.success && result.data) selectSkill(result.data.record.id);
    return result;
  }

  async function resyncAll() {
    try {
      const result = await window.crab.skills.resyncAll();
      if (!result.success)
        throw new Error(result.error ?? "Could not sync skills.");
      // Per-agent failures are reflected in actual link status, not the backend's count.
      const items = await refreshSkills();
      if (!items)
        throw new Error(
          "Sync finished, but its status could not be refreshed. Retry loading skills.",
        );
      return items.reduce(
        (count, item) =>
          count +
          Object.values(item.agentStatus).filter((status) => status === "error")
            .length,
        0,
      );
    } finally {
      void refreshAgents();
    }
  }

  return {
    state: {
      skills,
      loading,
      error,
      selectedSkillId,
      selectedSkill,
      detailLoading,
      detailError,
      detectedAgents,
      agentsLoading,
      agentsError,
    },
    refreshSkills,
    refreshAgents,
    selectSkill,
    createSkill,
    installFromUrl,
    resyncAll,
    retryDetail: () => setDetailRevision((value) => value + 1),
    updateSkill: (data: Parameters<typeof window.crab.skills.updateSkill>[0]) =>
      mutate(() => window.crab.skills.updateSkill(data)),
    deleteSkill: (id: string) =>
      mutate(() => window.crab.skills.deleteSkill(id)),
    toggleEnabled: (id: string, enabled: boolean) =>
      mutate(() => window.crab.skills.toggleEnabled(id, enabled)),
  };
}

const SkillsContext = createContext<ReturnType<typeof useSkillsState> | null>(
  null,
);

export function SkillsProvider({
  children,
  repoPath,
}: {
  children: ReactNode;
  repoPath?: string;
}) {
  const value = useSkillsState(repoPath);
  return (
    <SkillsContext.Provider value={value}>{children}</SkillsContext.Provider>
  );
}

export function useSkills() {
  const context = useContext(SkillsContext);
  if (!context) throw new Error("useSkills used outside SkillsProvider");
  return context;
}
