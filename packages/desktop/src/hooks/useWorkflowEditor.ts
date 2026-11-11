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
import { mainProcess, onAgentEvent, rpc } from "../ipc";

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
  | { type: "run_started"; run_id: string; stages: string[] }
  | { type: "stage_status"; name: string; status: StageRunStatus; duration_ms?: number; error?: string }
  | { type: "run_output"; text: string; error?: boolean }
  | { type: "clear_output" }
  | { type: "run_done"; status: "done" | "failed" | "cancelled"; error?: string }
  | { type: "set_error"; message: string }
  | { type: "reset_run" }
  | { type: "set_files"; files: string[]; path: string | null }
  | { type: "saved"; workflow: WorkflowDefinition };

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
      return { ...state, phase: "empty", workflow: null, activeFile: null, runState: null, dirty: false, availableFiles: action.files, error: null };

    case "loaded":
      return {
        ...state,
        phase: "editing",
        workflow: action.workflow,
        runState: null,
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
          output: [],
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

    case "run_output":
      if (!state.runState) return state;
      return {
        ...state,
        runState: {
          ...state.runState,
          output: [...state.runState.output, {
            id: (state.runState.output.at(-1)?.id ?? 0) + 1,
            text: action.text,
            error: action.error,
          }].slice(-500),
        },
      };

    case "clear_output":
      if (!state.runState) return state;
      return { ...state, runState: { ...state.runState, output: [] } };

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

    case "set_files":
      return { ...state, availableFiles: action.files, activeFile: action.path, workflow: null, dirty: false };

    case "saved":
      return state.workflow === action.workflow ? { ...state, dirty: false } : state;

    default:
      return state;
  }
}

// ── Hook ────────────────────────────────────────────────────────────

export function useWorkflowEditor(repoId: string, repoPath?: string) {
  const [state, dispatch] = useReducer(reducer, undefined, initState);
  const runIdRef = useRef<string | null>(null);
  const stateRef = useRef(state);
  stateRef.current = state;
  const [catalog, setCatalog] = useState<WorkflowDefinition[]>([]);
  const [fileErrors, setFileErrors] = useState<Record<string, string>>({});
  const [operationError, setOperationError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const saveQueue = useRef<Promise<unknown>>(Promise.resolve());
  const lastSaved = useRef<WorkflowDefinition | null>(null);
  const loadVersion = useRef(0);
  const autoSaveTimer = useRef<ReturnType<typeof setTimeout> | null>(null);

  // Serialize disk writes. A slow autosave must not overwrite a newer edit or
  // clear its dirty flag; navigation flushes the same queue before loading.
  const persist = useCallback(async (workflow: WorkflowDefinition) => {
    const saved = saveQueue.current.catch(() => {}).then(() => rpc.workflowSave(repoId, workflow));
    saveQueue.current = saved;
    await saved;
    lastSaved.current = workflow;
    dispatch({ type: "saved", workflow });
    setCatalog(files => files.map(file => file.path === workflow.path ? workflow : file));
  }, [repoId]);

  const flush = useCallback(async () => {
    if (autoSaveTimer.current) clearTimeout(autoSaveTimer.current);
    await saveQueue.current.catch(() => {});
    const current = stateRef.current;
    if (current.dirty && current.workflow && current.workflow !== lastSaved.current) await persist(current.workflow);
  }, [persist]);

  // Layout state (persisted in localStorage per workflow file).
  const [layout, setLayout] = useState<WorkflowLayout>({ positions: {} });

  // Additional state for status, DAG, and active tab.
  const [statusReport, setStatusReport] = useState<WorkflowStatusReport | null>(null);
  const [dagInfo, setDagInfo] = useState<DagInfo | null>(null);
  const [activeTab, setActiveTab] = useState<WorkflowTab>("editor");

  const loadLayout = useCallback((filePath: string) => {
    try {
      const key = `crab.workflow.layout.v2.${repoId}.${filePath}`;
      const raw = localStorage.getItem(key);
      if (raw) setLayout(JSON.parse(raw));
      else setLayout({ positions: {} });
    } catch {
      setLayout({ positions: {} });
    }
  }, [repoId]);

  const saveLayout = useCallback((filePath: string, positions: Record<string, NodePosition>) => {
    const key = `crab.workflow.layout.v2.${repoId}.${filePath}`;
    const l: WorkflowLayout = { positions };
    setLayout(l);
    localStorage.setItem(key, JSON.stringify(l));
  }, [repoId]);

  // Keep malformed workflows in the sidebar so another file remains reachable.
  const load = useCallback(async (filePath?: string) => {
    if (stateRef.current.phase === "running") return;
    const version = ++loadVersion.current;
    setBusy(true);
    setOperationError(null);
    let flushed = false;
    try {
      await flush();
      flushed = true;
      dispatch({ type: "set_loading" });
      const { files } = await rpc.workflowList(repoId);
      const entries = await Promise.all(files.map(async path => {
        try { return { path, workflow: await rpc.workflowGet(repoId, path) }; }
        catch (error) { return { path, error: error instanceof Error ? error.message : String(error) }; }
      }));
      if (version !== loadVersion.current) return;
      setCatalog(entries.flatMap(entry => entry.workflow ? [entry.workflow] : []));
      setFileErrors(Object.fromEntries(entries.flatMap(entry => entry.error ? [[entry.path, entry.error]] : [])));
      setStatusReport(null);
      setDagInfo(null);
      runIdRef.current = null;
      if (!files.length) { dispatch({ type: "set_empty", files }); return; }
      const target = filePath ?? (files.includes(stateRef.current.activeFile ?? "") ? stateRef.current.activeFile : files[0]);
      const entry = entries.find(entry => entry.path === target);
      dispatch({ type: "set_files", files, path: target });
      if (!entry?.workflow) throw new Error(entry?.error ?? `Workflow not found: ${target}`);
      dispatch({ type: "loaded", workflow: entry.workflow, files });
      loadLayout(entry.path);
    } catch (error) {
      if (version === loadVersion.current) {
        const message = error instanceof Error ? error.message : String(error);
        if (!flushed) setOperationError(message);
        else dispatch({ type: "set_error", message });
      }
    } finally { if (version === loadVersion.current) setBusy(false); }
  }, [repoId, loadLayout, flush]);

  useEffect(() => { void load(); }, [load]);

  const save = useCallback(async () => {
    const workflow = stateRef.current.workflow;
    if (!workflow) return;
    try { await persist(workflow); setOperationError(null); }
    catch (error) { setOperationError(`Save failed: ${error instanceof Error ? error.message : String(error)}`); }
  }, [persist]);

  // Auto-save: debounce saves when dirty.
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

    if (state.phase === "running") return;
    setBusy(true);
    setOperationError(null);
    try { await flush(); }
    catch (error) {
      setOperationError(`Save failed: ${error instanceof Error ? error.message : String(error)}`);
      setBusy(false);
      return;
    }
    setBusy(false);

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
  }, [repoId, state.workflow, state.phase, fetchStatus, flush]);

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

  const manage = async (operation: () => Promise<unknown>, target?: string) => {
    if (stateRef.current.phase === "running") return false;
    setBusy(true);
    setOperationError(null);
    try {
      await flush();
      await operation();
      await load(target);
      return true;
    } catch (error) {
      setOperationError(error instanceof Error ? error.message : String(error));
      return false;
    } finally { setBusy(false); }
  };

  const deleteWorkflow = (path: string) => manage(() => rpc.workflowDelete(repoId, path));
  const createWorkflow = (path: string) => manage(() => rpc.workflowCreate(repoId, path), path);
  const renameWorkflow = (oldPath: string, newPath: string) => manage(async () => {
    if (!repoPath) throw new Error("Rename requires a local working directory.");
    const prefix = repoPath.replace(/[\\/]$/, "") + "/";
    await mainProcess.fsRename(repoId, prefix + oldPath, prefix + newPath);
  }, newPath);

  // Subscribe to workflow execution events.
  useEffect(() => {
    const unsub = onAgentEvent((evt) => {
      // Own logs alongside run state: fast CLI notifications can arrive before
      // the output pane renders, and another repository's jobs must stay separate.
      if (evt.method === "progress/line") {
        const line = evt.params as { job_id?: string; line?: string };
        if (line.job_id === runIdRef.current && typeof line.line === "string") {
          dispatch({ type: "run_output", text: line.line });
        }
        return;
      }
      if (evt.method !== "workflow/event") return;
      const { job_id, schema, data } = evt.params as { job_id: string; schema: string; data: Record<string, unknown> | null };
      // Terminal error frames carry `error` instead of stage `data`; the RPC
      // rejection reports them without treating them as a stage update.
      if (job_id !== runIdRef.current || !data) return;
      const statuses: Record<string, StageRunStatus> = {
        "workflow.stage.started": "running",
        "workflow.stage.running": "running",
        "workflow.stage.committed": data.cache_hit ? "cached" : "done",
        "workflow.stage.failed": "failed",
        "workflow.stage.not_started": "pending",
      };
      const status = statuses[schema];
      if (!status || typeof data.stage !== "string") return;
      // Recursive CLI stage names include the workflow directory/name prefix.
      const parts = state.workflow?.path.replace(/\\/g, "/").split("/") ?? [];
      const filename = parts.pop();
      if (filename?.endsWith(".workflow.yaml")) parts.push(filename.slice(0, -".workflow.yaml".length));
      const prefix = parts.filter(part => part && part !== ".").join(".");
      const stage = state.workflow?.stages.find(({ name }) => data.stage === (prefix ? `${prefix}.${name}` : name));
      const labels: Partial<Record<StageRunStatus, string>> = {
        running: "Started", cached: "Cached", done: "Completed", failed: "Failed", pending: "Not started",
      };
      const reason = status === "failed" && typeof data.reason === "string" ? `: ${data.reason}` : "";
      dispatch({ type: "run_output", text: `${stage?.name ?? data.stage}: ${labels[status]}${reason}`, error: status === "failed" });
      if (!stage) return;
      dispatch({ type: "stage_status", name: stage.name, status,
        duration_ms: data.duration_ms as number | undefined,
        error: data.reason as string | undefined });
    });
    return unsub;
  }, [state.workflow]);

  return {
    state,
    catalog,
    fileErrors,
    operationError,
    busy,
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
      renameWorkflow,
      addStage: (stage: WorkflowStage) => dispatch({ type: "add_stage", stage }),
      updateStage: (stage: WorkflowStage) => dispatch({ type: "update_stage", stage }),
      renameStage: (oldName: string, newName: string) => dispatch({ type: "rename_stage", oldName, newName }),
      removeStage: (name: string) => dispatch({ type: "remove_stage", name }),
      setStages: (stages: WorkflowStage[]) => dispatch({ type: "set_stages", stages }),
      resetRun: () => dispatch({ type: "reset_run" }),
      clearOutput: () => dispatch({ type: "clear_output" }),
      switchFile: load,
      fetchStatus,
      fetchDag,
      setActiveTab,
      runOutdated,
    },
  };
}
