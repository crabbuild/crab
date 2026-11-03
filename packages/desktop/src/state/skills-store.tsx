import React, { createContext, useContext, useReducer, useCallback, useEffect, useMemo } from "react";
import type { SkillListItem, SkillDetail, DetectedAgent, SkillOpResult } from "../ipc";

// ── State ──
export interface SkillsState {
  skills: SkillListItem[];
  loading: boolean;
  error: string | null;
  selectedSkillId: string | null;
  selectedSkill: SkillDetail | null;
  detailLoading: boolean;
  detectedAgents: DetectedAgent[];
  agentsLoading: boolean;
}

const initialState: SkillsState = {
  skills: [],
  loading: false,
  error: null,
  selectedSkillId: null,
  selectedSkill: null,
  detailLoading: false,
  detectedAgents: [],
  agentsLoading: false,
};

// ── Actions ──
type SkillsAction =
  | { type: "SET_LOADING"; loading: boolean }
  | { type: "SET_ERROR"; error: string | null }
  | { type: "SET_SKILLS"; skills: SkillListItem[] }
  | { type: "SET_SELECTED_ID"; id: string | null }
  | { type: "SET_SELECTED_SKILL"; skill: SkillDetail | null; loading: boolean }
  | { type: "SET_AGENTS"; agents: DetectedAgent[] }
  | { type: "ADD_SKILL"; skill: SkillListItem }
  | { type: "UPDATE_SKILL"; skill: SkillListItem }
  | { type: "REMOVE_SKILL"; id: string };

function reducer(state: SkillsState, action: SkillsAction): SkillsState {
  switch (action.type) {
    case "SET_LOADING":
      return { ...state, loading: action.loading };
    case "SET_ERROR":
      return { ...state, error: action.error };
    case "SET_SKILLS":
      return { ...state, skills: action.skills, loading: false };
    case "SET_SELECTED_ID":
      return { ...state, selectedSkillId: action.id, selectedSkill: null };
    case "SET_SELECTED_SKILL":
      return { ...state, selectedSkill: action.skill, detailLoading: action.loading };
    case "SET_AGENTS":
      return { ...state, detectedAgents: action.agents, agentsLoading: false };
    case "ADD_SKILL":
      return { ...state, skills: [action.skill, ...state.skills] };
    case "UPDATE_SKILL":
      return {
        ...state,
        skills: state.skills.map((s) =>
          s.record.id === action.skill.record.id ? action.skill : s,
        ),
      };
    case "REMOVE_SKILL":
      return {
        ...state,
        skills: state.skills.filter((s) => s.record.id !== action.id),
        selectedSkillId: state.selectedSkillId === action.id ? null : state.selectedSkillId,
        selectedSkill: state.selectedSkillId === action.id ? null : state.selectedSkill,
      };
    default:
      return state;
  }
}

// ── Context ──
interface SkillsContextType {
  state: SkillsState;
  dispatch: React.Dispatch<SkillsAction>;
  refreshSkills: (opts?: { scope?: string; repoPath?: string }) => Promise<void>;
  selectSkill: (id: string | null) => Promise<void>;
  createSkill: (data: {
    name: string;
    description: string;
    body?: string;
    scope: string;
    repoPath?: string;
  }) => Promise<SkillOpResult<SkillDetail>>;
  updateSkill: (data: {
    id: string;
    updates: { name?: string; description?: string; body?: string };
  }) => Promise<SkillOpResult<SkillDetail>>;
  deleteSkill: (id: string) => Promise<SkillOpResult<void>>;
  installFromUrl: (url: string) => Promise<SkillOpResult<SkillDetail>>;
  toggleEnabled: (id: string, enabled: boolean) => Promise<void>;
  setActivationPrefs: (id: string, prefs: { autoActivate: boolean; restrictToAgents: string[] }) => Promise<void>;
  resyncAll: () => Promise<void>;
}

const SkillsContext = createContext<SkillsContextType | null>(null);

export function SkillsProvider({
  children,
  repoPath,
}: {
  children: React.ReactNode;
  repoPath?: string;
}) {
  const [state, dispatch] = useReducer(reducer, initialState);

  const refreshSkills = useCallback(
    async (opts?: { scope?: string; repoPath?: string }) => {
      dispatch({ type: "SET_LOADING", loading: true });
      try {
        const result = await window.crab.skills.listSkills(opts);
        if (result.success && result.data) {
          dispatch({ type: "SET_SKILLS", skills: result.data });
        } else {
          dispatch({ type: "SET_ERROR", error: result.error ?? "Failed to load skills" });
        }
      } catch (e) {
        dispatch({ type: "SET_ERROR", error: (e as Error).message });
      }
    },
    [],
  );

  const selectSkill = useCallback(async (id: string | null) => {
    dispatch({ type: "SET_SELECTED_ID", id });
    if (!id) return;

    dispatch({ type: "SET_SELECTED_SKILL", skill: null, loading: true });
    try {
      const result = await window.crab.skills.getSkill(id);
      if (result.success && result.data) {
        dispatch({ type: "SET_SELECTED_SKILL", skill: result.data, loading: false });
      } else {
        dispatch({ type: "SET_SELECTED_SKILL", skill: null, loading: false });
      }
    } catch {
      dispatch({ type: "SET_SELECTED_SKILL", skill: null, loading: false });
    }
  }, []);

  const createSkillFn = useCallback(
    async (data: {
      name: string;
      description: string;
      body?: string;
      scope: string;
      repoPath?: string;
    }) => {
      const effectiveRepoPath = data.repoPath ?? repoPath;
      const result = await window.crab.skills.createSkill({ ...data, repoPath: effectiveRepoPath } as any);
      if (result.success) {
        await refreshSkills(repoPath ? { repoPath } : undefined);
      }
      return result;
    },
    [refreshSkills, repoPath],
  );

  const updateSkillFn = useCallback(
    async (data: {
      id: string;
      updates: { name?: string; description?: string; body?: string };
    }) => {
      const result = await window.crab.skills.updateSkill(data as any);
      if (result.success) {
        await refreshSkills(repoPath ? { repoPath } : undefined);
        if (state.selectedSkillId === data.id) {
          await selectSkill(data.id);
        }
      }
      return result;
    },
    [refreshSkills, selectSkill, state.selectedSkillId, repoPath],
  );

  const deleteSkillFn = useCallback(
    async (id: string) => {
      const result = await window.crab.skills.deleteSkill(id);
      if (result.success) {
        await refreshSkills(repoPath ? { repoPath } : undefined);
      }
      return result;
    },
    [refreshSkills, repoPath],
  );

  const installFromUrlFn = useCallback(
    async (url: string) => {
      const result = await window.crab.skills.installFromUrl(url, "user", repoPath);
      if (result.success) {
        await refreshSkills(repoPath ? { repoPath } : undefined);
      }
      return result;
    },
    [refreshSkills, repoPath],
  );

  const toggleEnabledFn = useCallback(
    async (id: string, enabled: boolean) => {
      await window.crab.skills.toggleEnabled(id, enabled);
      await refreshSkills(repoPath ? { repoPath } : undefined);
    },
    [refreshSkills, repoPath],
  );

  const setActivationPrefsFn = useCallback(
    async (id: string, prefs: { autoActivate: boolean; restrictToAgents: string[] }) => {
      await window.crab.skills.setActivationPrefs(id, prefs as any);
      await refreshSkills(repoPath ? { repoPath } : undefined);
    },
    [refreshSkills, repoPath],
  );

  const resyncAllFn = useCallback(async () => {
    await window.crab.skills.resyncAll();
    await refreshSkills(repoPath ? { repoPath } : undefined);
  }, [refreshSkills, repoPath]);

  // Load on mount
  useEffect(() => {
    refreshSkills(repoPath ? { repoPath } : undefined);

    // Load detected agents
    dispatch({ type: "SET_AGENTS", agents: [] });
    window.crab.skills
      .detectAgents()
      .then((agents) => dispatch({ type: "SET_AGENTS", agents }))
      .catch(() => dispatch({ type: "SET_AGENTS", agents: [] }));
  }, [refreshSkills, repoPath]);

  const value = useMemo(
    () => ({
      state,
      dispatch,
      refreshSkills,
      selectSkill,
      createSkill: createSkillFn,
      updateSkill: updateSkillFn,
      deleteSkill: deleteSkillFn,
      installFromUrl: installFromUrlFn,
      toggleEnabled: toggleEnabledFn,
      setActivationPrefs: setActivationPrefsFn,
      resyncAll: resyncAllFn,
    }),
    [
      state,
      refreshSkills,
      selectSkill,
      createSkillFn,
      updateSkillFn,
      deleteSkillFn,
      installFromUrlFn,
      toggleEnabledFn,
      setActivationPrefsFn,
      resyncAllFn,
    ],
  );

  return (
    <SkillsContext.Provider value={value}>{children}</SkillsContext.Provider>
  );
}

export function useSkills(): SkillsContextType {
  const ctx = useContext(SkillsContext);
  if (!ctx) throw new Error("useSkills used outside SkillsProvider");
  return ctx;
}
