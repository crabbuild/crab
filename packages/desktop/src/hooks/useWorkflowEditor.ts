import { useCallback, useEffect, useReducer, useRef, useState } from "react";
import {
  WorkflowDefinition,
  WorkflowRunState,
  WorkflowStage,
  StageRunState,
  StageRunStatus,
  WorkflowLayout,
  NodePosition,
  WorkflowStatusReport,
  DagInfo,
} from "../lib/workflow-types";
import type { WorkflowTab } from "../components/workflow/WorkflowTabs";
import { onAgentEvent, rpc } from "../ipc";

// Auto-save debounce delay in milliseconds.
const AUTO_SAVE_DELAY_MS = 800;

// ── State machine ───────────────────────────────────────────────────

export type WorkflowEditorPhase =
  | "loading"
  | "empty"
  | "editing"
  | "running"
  | "done"
  | "error";

export interface WorkflowEditorState {
  phase: WorkflowEditorPhase;
  workflow: WorkflowDefinition | null;
  runState: WorkflowRunState | null;
  dirty: boolean;
  error: string | null;
  /** Available workflow files in the repo. */
  availableFiles: string[];
  /** Currently loaded file path. */
  activeFile: string | null;
}

type Action =
  | { type: "set_loading" }
  | { type: "set_empty"; files: string[] }
  | { type: "loaded"; workflow: WorkflowDefinition; files: string[] }
  | { type: "update_stage"; stage: WorkflowStage }
  | { type: "rename_stage"; oldName: string; newName: string }
  | { type: "add_stage"; stage: WorkflowStage }
  | { type: "remove_stage"; name: string }
  | { type: "set_stages"; stages: WorkflowStage[] }
  | { type: "set_dirty"; dirty: boolean }
  | { type: "run_started"; run_id: string; stages: string[] }
  | { type: "stage_status"; name: string; status: StageRunStatus; duration_ms?: number; error?: string }
  | { type: "run_done"; status: "done" | "failed" | "cancelled"; error?: string }
  | { type: "set_error"; message: string }
  | { type: "reset_run" }
  | { type: "set_active_file"; path: string };

function initState(): WorkflowEditorState {
  return {
    phase: "loading",
    workflow: null,
    runState: null,
    dirty: false,
    error: null,
    availableFiles: [],
    activeFile: null,
  };
}

function reducer(state: WorkflowEditorState, action: Action): WorkflowEditorState {
  switch (action.type) {
    case "set_loading":
      return { ...state, phase: "loading", error: null };

    case "set_empty":
      return { ...state, phase: "empty", workflow: null, availableFiles: action.files, error: null };

    case "loaded":
      return {
        ...state,
        phase: "editing",
        workflow: action.workflow,
        availableFiles: action.files,
        activeFile: action.workflow.path,
        dirty: false,
        error: null,
      };

    case "update_stage": {
      if (!state.workflow) return state;
      const stages = state.workflow.stages.map((s) =>
        s.name === action.stage.name ? action.stage : s,
      );
      return {
        ...state,
        workflow: { ...state.workflow, stages },
        dirty: true,
      };
    }

    case "rename_stage": {
      if (!state.workflow) return state;
      const { oldName, newName } = action as { type: "rename_stage"; oldName: string; newName: string };
      const stages = state.workflow.stages.map((s) => {
        // Rename the stage itself.
        if (s.name === oldName) {
          return { ...s, name: newName };
        }
        // Update deps that reference the old name.
        const deps = s.deps.map((d) => {
          if (d.kind === "stage_out" && d.value.startsWith(`${oldName}:`)) {
            return { ...d, value: `${newName}:${d.value.slice(oldName.length + 1)}` };
          }
          if (d.kind === "path") {
            const colonIdx = d.value.indexOf(":");
            if (colonIdx > 0 && d.value.slice(0, colonIdx) === oldName) {
              return { ...d, value: `${newName}:${d.value.slice(colonIdx + 1)}` };
            }
          }
          return d;
        });
        return { ...s, deps };
      });
      return {
        ...state,
        workflow: { ...state.workflow, stages },
        dirty: true,
      };
    }

    case "add_stage": {
      if (!state.workflow) return state;
      return {
        ...state,
        workflow: { ...state.workflow, stages: [...state.workflow.stages, action.stage] },
        dirty: true,
      };
    }

    case "remove_stage": {
      if (!state.workflow) return state;
      const stages = state.workflow.stages.filter((s) => s.name !== action.name);
      return {
        ...state,
        workflow: { ...state.workflow, stages },
        dirty: true,
      };
    }

    case "set_stages": {
      if (!state.workflow) return state;
      return {
        ...state,
        workflow: { ...state.workflow, stages: action.stages },
        dirty: true,
      };
    }

    case "set_dirty":
      return { ...state, dirty: action.dirty };

    case "run_started": {
      const stages: StageRunState[] = action.stages.map((name) => ({
        name,
        status: "pending",
      }));
      return {
        ...state,
        phase: "running",
        runState: {
          run_id: action.run_id,
          status: "running",
          stages,
          started_at: Date.now(),
        },
      };
    }

    case "stage_status": {
      if (!state.runState) return state;
      const stages = state.runState.stages.map((s) =>
        s.name === action.name
          ? { ...s, status: action.status, duration_ms: action.duration_ms, error: action.error }
          : s,
      );
      return {
        ...state,
        runState: { ...state.runState, stages },
      };
    }

    case "run_done":
      if (!state.runState) return state;
      return {
        ...state,
        phase: "done",
        runState: {
          ...state.runState,
          status: action.status,
          finished_at: Date.now(),
          error: action.error,
        },
      };

    case "reset_run":
      return { ...state, phase: "editing", runState: null };

    case "set_error":
      return { ...state, phase: "error", error: action.message };

    case "set_active_file":
      return { ...state, activeFile: action.path };

    default:
      return state;
  }
}

// ── Hook ────────────────────────────────────────────────────────────

export function useWorkflowEditor(repoId: string) {
  const [state, dispatch] = useReducer(reducer, undefined, initState);
  const runIdRef = useRef<string | null>(null);

  // Layout state (persisted in localStorage per workflow file).
  const [layout, setLayout] = useState<WorkflowLayout>({ positions: {} });

  // Additional state for status, DAG, and active tab.
  const [statusReport, setStatusReport] = useState<WorkflowStatusReport | null>(null);
  const [dagInfo, setDagInfo] = useState<DagInfo | null>(null);
  const [activeTab, setActiveTab] = useState<WorkflowTab>("editor");

  const loadLayout = useCallback((filePath: string) => {
    try {
      const key = `crab.workflow.layout.${repoId}.${filePath}`;
      const raw = localStorage.getItem(key);
      if (raw) setLayout(JSON.parse(raw));
      else setLayout({ positions: {} });
    } catch {
      setLayout({ positions: {} });
    }
  }, [repoId]);

  const saveLayout = useCallback((filePath: string, positions: Record<string, NodePosition>) => {
    const key = `crab.workflow.layout.${repoId}.${filePath}`;
    const l: WorkflowLayout = { positions };
    setLayout(l);
    localStorage.setItem(key, JSON.stringify(l));
  }, [repoId]);

  // Load workflow list + active workflow.
  const load = useCallback(async (filePath?: string) => {
    dispatch({ type: "set_loading" });
    try {
      const { files } = await rpc.workflowList(repoId);
      if (files.length === 0) {
        dispatch({ type: "set_empty", files: [] });
        return;
      }
      const target = filePath ?? files[0];
      const workflow = await rpc.workflowGet(repoId, target);
      dispatch({ type: "loaded", workflow, files });
      loadLayout(target);
    } catch (err) {
      dispatch({ type: "set_error", message: err instanceof Error ? err.message : String(err) });
    }
  }, [repoId, loadLayout]);

  // Initial load.
  useEffect(() => {
    load();
  }, [load]);

  // Save workflow to disk.
  const save = useCallback(async () => {
    if (!state.workflow) return;
    try {
      await rpc.workflowSave(repoId, state.workflow);
      dispatch({ type: "set_dirty", dirty: false });
      // Fetch DAG after save.
      if (state.workflow.path) {
        try {
          const dag = await rpc.workflowDag(repoId, state.workflow.path);
          setDagInfo(dag);
        } catch {
          // Non-critical — DAG fetch failure shouldn't block save.
        }
      }
    } catch (err) {
      dispatch({ type: "set_error", message: err instanceof Error ? err.message : String(err) });
    }
  }, [repoId, state.workflow]);

  // Auto-save: debounce saves when dirty.
  const autoSaveTimer = useRef<ReturnType<typeof setTimeout> | null>(null);
  useEffect(() => {
    if (!state.dirty || !state.workflow) return;
    if (autoSaveTimer.current) clearTimeout(autoSaveTimer.current);
    autoSaveTimer.current = setTimeout(() => {
      save();
    }, AUTO_SAVE_DELAY_MS);
    return () => {
      if (autoSaveTimer.current) clearTimeout(autoSaveTimer.current);
    };
  }, [state.dirty, state.workflow, save]);

  // Fetch workflow status report.
  const fetchStatus = useCallback(async () => {
    if (!state.activeFile) return;
    try {
      const report = await rpc.workflowStatus(repoId, state.activeFile);
      setStatusReport(report);
    } catch {
      // Non-critical.
    }
  }, [repoId, state.activeFile]);

  // Fetch DAG info.
  const fetchDag = useCallback(async () => {
    if (!state.activeFile) return;
    try {
      const dag = await rpc.workflowDag(repoId, state.activeFile);
      setDagInfo(dag);
    } catch {
      // Non-critical.
    }
  }, [repoId, state.activeFile]);

  // Run workflow. Auto-saves if dirty before running.
  const run = useCallback(async (stages?: string[]) => {
    if (!state.workflow) return;

    // Auto-save before running if there are unsaved changes.
    if (state.dirty) {
      try {
        await rpc.workflowSave(repoId, state.workflow);
        dispatch({ type: "set_dirty", dirty: false });
      } catch (err) {
        dispatch({ type: "set_error", message: `Save failed: ${err instanceof Error ? err.message : String(err)}` });
        return;
      }
    }

    // Determine stage names from local state so we can show "running"
    // immediately without waiting for the RPC response.
    const stageNames = stages ?? state.workflow.stages.map((s) => s.name);
    const tempRunId = crypto.randomUUID();
    runIdRef.current = tempRunId;
    dispatch({ type: "run_started", run_id: tempRunId, stages: stageNames });

    try {
      await rpc.workflowRun(repoId, state.workflow.path, tempRunId, stages);
      if (runIdRef.current !== tempRunId) return;
      dispatch({ type: "run_done", status: "done" });
      await fetchStatus();
    } catch (err) {
      if (runIdRef.current !== tempRunId) return;
      dispatch({ type: "run_done", status: "failed", error: err instanceof Error ? err.message : String(err) });
    }
  }, [repoId, state.workflow, state.dirty, fetchStatus]);

  // Run outdated stages only.
  const runOutdated = useCallback(async (stages: string[]) => {
    if (!state.workflow || stages.length === 0) return;
    await run(stages);
  }, [state.workflow, run]);

  // Cancel running workflow.
  const cancel = useCallback(async () => {
    if (!state.runState?.run_id) return;
    try {
      const { cancelled } = await rpc.workflowCancel(repoId, state.runState.run_id);
      if (cancelled) {
        runIdRef.current = null;
        dispatch({ type: "run_done", status: "cancelled" });
      }
    } catch {
      // Best-effort cancel.
    }
  }, [repoId, state.runState]);

  // Delete a workflow file.
  const deleteWorkflow = useCallback(async (filePath: string) => {
    try {
      await rpc.workflowDelete(repoId, filePath);
      await load();
    } catch (err) {
      dispatch({ type: "set_error", message: err instanceof Error ? err.message : String(err) });
    }
  }, [repoId, load]);

  // Create a new workflow file.
  const createWorkflow = useCallback(async (filePath: string) => {
    try {
      await rpc.workflowCreate(repoId, filePath);
      await load(filePath);
    } catch (err) {
      dispatch({ type: "set_error", message: err instanceof Error ? err.message : String(err) });
    }
  }, [repoId, load]);

  // Subscribe to workflow execution events.
  useEffect(() => {
    const unsub = onAgentEvent((evt) => {
      if (evt.method !== "workflow/event") return;
      const event = evt.params as { job_id: string; schema: string; data: Record<string, unknown> };
      if (event.job_id !== runIdRef.current) return;
      const statuses: Record<string, StageRunStatus> = {
        "workflow.stage.started": "running",
        "workflow.stage.running": "running",
        "workflow.stage.committed": event.data.cache_hit ? "cached" : "done",
        "workflow.stage.failed": "failed",
        "workflow.stage.not_started": "pending",
      };
      const status = statuses[event.schema];
      if (!status || typeof event.data.stage !== "string") return;
      // Recursive CLI stage names include the workflow directory/name prefix.
      const parts = state.workflow?.path.replace(/\\/g, "/").split("/") ?? [];
      const filename = parts.pop();
      if (filename?.endsWith(".workflow.yaml")) parts.push(filename.slice(0, -".workflow.yaml".length));
      const prefix = parts.filter(part => part && part !== ".").join(".");
      const stage = state.workflow?.stages.find(({ name }) => event.data.stage === (prefix ? `${prefix}.${name}` : name));
      if (!stage) return;
      dispatch({ type: "stage_status", name: stage.name, status,
        duration_ms: event.data.duration_ms as number | undefined,
        error: event.data.reason as string | undefined });
    });
    return unsub;
  }, [state.workflow]);

  return {
    state,
    dispatch,
    layout,
    statusReport,
    dagInfo,
    activeTab,
    saveLayout: (positions: Record<string, NodePosition>) => {
      if (state.activeFile) saveLayout(state.activeFile, positions);
    },
    actions: {
      load,
      save,
      run,
      cancel,
      deleteWorkflow,
      createWorkflow,
      addStage: (stage: WorkflowStage) => dispatch({ type: "add_stage", stage }),
      updateStage: (stage: WorkflowStage) => dispatch({ type: "update_stage", stage }),
      renameStage: (oldName: string, newName: string) => dispatch({ type: "rename_stage", oldName, newName }),
      removeStage: (name: string) => dispatch({ type: "remove_stage", name }),
      setStages: (stages: WorkflowStage[]) => dispatch({ type: "set_stages", stages }),
      resetRun: () => dispatch({ type: "reset_run" }),
      switchFile: (path: string) => {
        dispatch({ type: "set_active_file", path });
        load(path);
      },
      fetchStatus,
      fetchDag,
      setActiveTab,
      runOutdated,
    },
  };
}
