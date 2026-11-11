// ─── Imports ─────────────────────────────────────────────────────────────────

import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  ReactFlow,
  Background,
  Controls,
  MiniMap,
  Panel,
  useNodesState,
  useEdgesState,
  Connection,
  Node,
  Edge,
  NodeTypes,
  MarkerType,
  useReactFlow,
  useStore,
  useNodesInitialized,
  ReactFlowProvider,
} from "@xyflow/react";
import clsx from "clsx";
import {
  AlertTriangle,
  ChevronDown,
  Copy,
  Code2,
  FilePlus,
  HelpCircle,
  LayoutGrid,
  Loader2,
  Play,
  Plus,
  Redo2,
  Save,
  Snowflake,
  Square,
  Trash2,
  Undo2,
  Maximize2,
} from "lucide-react";
import { OpenRepo } from "../state";
import { useWorkflowEditor } from "../hooks/useWorkflowEditor";
import { WorkflowStage, WorkflowRunState } from "../lib/workflow-types";
import { StageNode } from "../components/workflow/StageNode";
import { StagePropertiesPanel } from "../components/workflow/StagePropertiesPanel";
import { NewStageDialog } from "../components/workflow/NewStageDialog";
import { WorkflowSidebar } from "../components/workflow/WorkflowSidebar";
import { artifactPath, availableWorkflowOutputs, cycleStages, layoutStages, stageEdges, stageRunStatus, workflowLinks } from "../lib/workflow-graph";
import { WorkflowTabs } from "../components/workflow/WorkflowTabs";
import { StatusDashboard } from "../components/workflow/StatusDashboard";
import { ExperimentPanel } from "../components/workflow/ExperimentPanel";
import { MigrateWizard } from "../components/workflow/MigrateWizard";

// ─── Types ───────────────────────────────────────────────────────────────────

interface Props {
  repo: OpenRepo;
  onOpenSource: (path: string) => Promise<void>;
}

/** A snapshot of the workflow stages at a point in time, used for undo/redo. */
interface HistoryEntry {
  stages: WorkflowStage[];
  label: string;
}

/** Describes the context menu state: position, kind, and optional target ID. */
interface ContextMenu {
  x: number;
  y: number;
  kind: "node" | "edge" | "pane";
  nodeId?: string;
  edgeId?: string;
}

// ─── Constants ───────────────────────────────────────────────────────────────

const nodeTypes: NodeTypes = { stage: StageNode };
// React Flow stores prop options beside queued fit requests. A new object on
// selection would overwrite Find stage's node filter before the request runs.
const fitViewOptions = { padding: 0.2, maxZoom: 1 };

// ─── Undo/Redo Hook ──────────────────────────────────────────────────────────

function useUndoRedo() {
  const [past, setPast] = useState<HistoryEntry[]>([]);
  const [future, setFuture] = useState<HistoryEntry[]>([]);

  const push = useCallback((stages: WorkflowStage[], label: string) => {
    setPast((previous) => [...previous.slice(-49), { stages, label }]);
    setFuture([]);
  }, []);

  const undo = useCallback(
    (currentStages: WorkflowStage[]): WorkflowStage[] | null => {
      if (!past.length) return null;
      const previousEntry = past[past.length - 1];
      setPast((entries) => entries.slice(0, -1));
      setFuture((entries) => [...entries, { stages: currentStages, label: "r" }]);
      return previousEntry.stages;
    },
    [past],
  );

  const redo = useCallback(
    (currentStages: WorkflowStage[]): WorkflowStage[] | null => {
      if (!future.length) return null;
      const nextEntry = future[future.length - 1];
      setFuture((entries) => entries.slice(0, -1));
      setPast((entries) => [...entries, { stages: currentStages, label: "u" }]);
      return nextEntry.stages;
    },
    [future],
  );

  return {
    push,
    undo,
    redo,
    clear: useCallback(() => { setPast([]); setFuture([]); }, []),
    canUndo: past.length > 0,
    canRedo: future.length > 0,
  };
}


// ─── Utility: Edge ID Parsing ────────────────────────────────────────────────

/** Parses an edge ID like "source->target" or "source->target:extra" into source/target. */
function parseEdgeId(id: string): { source: string; target: string } | null {
  const match = id.match(/^(.+)->(.+?)(?::.*)?$/);
  return match ? { source: match[1], target: match[2] } : null;
}

// ─── Main Component ──────────────────────────────────────────────────────────

function WorkflowEditorInner({ repo, onOpenSource }: Props) {
  const { state, catalog, fileErrors, operationError, busy, cancelling, layout, saveLayout, actions, statusReport, activeTab } =
    useWorkflowEditor(repo.repo_id, repo.path);
  const locked = busy || state.phase === "running";

  // ReactFlow state
  const [nodes, setNodes, onNodesChange] = useNodesState<Node>([]);
  const [edges, setEdges, onEdgesChange] = useEdgesState<Edge>([]);

  // Selection state
  const [selectedStage, setSelectedStage] = useState<string | null>(null);
  const [focusTarget, setFocusTarget] = useState<string | null>(null);
  const [selectedEdge, setSelectedEdge] = useState<string | null>(null);

  // Dialog visibility
  const [showNewDialog, setShowNewDialog] = useState(false);
  const [showMigrateWizard, setShowMigrateWizard] = useState(false);
  const [editorError, setEditorError] = useState<string | null>(null);
  const [showLegend, setShowLegend] = useState(false);

  // Context menu
  const [contextMenu, setContextMenu] = useState<ContextMenu | null>(null);

  // Refs and hooks
  const reactFlow = useReactFlow();
  const viewportWidth = useStore(state => state.width);
  const canvasRef = useRef<HTMLDivElement>(null);
  const fitViewDone = useRef<string | null>(null);
  const nodesInitialized = useNodesInitialized();
  const history = useUndoRedo();

  // Derived state
  const cycleNodes = useMemo(
    () => (state.workflow ? cycleStages(state.workflow.stages, state.workflow.executions) : new Set<string>()),
    [state.workflow],
  );

  // Helper: push history before a mutation
  const withHistory = useCallback(
    (label: string, mutation: () => void) => {
      if (locked) return;
      if (state.workflow) {
        history.push(state.workflow.stages, label);
      }
      mutation();
    },
    [state.workflow, history, locked],
  );

  // ─── Graph Building ──────────────────────────────────────────────────────

  const buildGraph = useCallback(() => {
    if (!state.workflow) {
      setNodes([]);
      setEdges([]);
      return;
    }

    const stages = state.workflow.stages;
    const positions = layoutStages(stages, layout.positions, state.workflow.executions);

    // Build run status lookup
    const statusMap = new Map(stages.map(stage => [stage.name, stageRunStatus(stage.name, state.workflow?.executions, state.runState)]));

    // Create nodes
    setNodes(
      stages.map((stage) => ({
        id: stage.name,
        type: "stage",
        position: positions[stage.name] ?? { x: 0, y: 0 },
        data: {
          stage,
          status: statusMap.get(stage.name) ?? "pending",
          selected: selectedStage === stage.name,
          hasCycle: cycleNodes.has(stage.name),
          executions: state.workflow?.executions?.filter(execution => execution.source === stage.name).length,
        },
      })),
    );

    setEdges(stageEdges(stages, state.workflow.executions).map(({ source, target }) => {
      const cycle = cycleNodes.has(source) && cycleNodes.has(target);
      const color = cycle ? "rgb(var(--gx-error))" : "rgb(var(--gx-muted))";
      return {
        id: `${source}->${target}`, source, target, type: "smoothstep",
        animated: statusMap.get(source) === "running",
        markerEnd: { type: MarkerType.ArrowClosed, color },
        style: { stroke: color, strokeWidth: 1.5 },
      };
    }));
  }, [state.workflow, state.runState, layout.positions, selectedStage, cycleNodes, setNodes, setEdges]);

  useEffect(() => {
    buildGraph();
  }, [buildGraph]);

  useEffect(() => {
    setSelectedStage(null);
    setFocusTarget(null);
    setSelectedEdge(null);
    setContextMenu(null);
    setEditorError(null);
    history.clear();
  }, [state.activeFile, history.clear]);

  useEffect(() => {
    if (nodesInitialized && activeTab === "editor" && state.activeFile !== fitViewDone.current) {
      const frame = requestAnimationFrame(() => {
        void reactFlow.fitView({ padding: 0.2, maxZoom: 1 });
        fitViewDone.current = state.activeFile;
      });
      return () => cancelAnimationFrame(frame);
    }
  }, [nodesInitialized, state.activeFile, activeTab, layout.positions, reactFlow]);

  useEffect(() => {
    const width = canvasRef.current?.clientWidth;
    if (!focusTarget || !nodesInitialized || width === undefined || Math.abs(width - viewportWidth) > 1) return;
    // Opening the inspector shrinks the canvas. Wait for React Flow's measured
    // width so the selected node is centered in the visible canvas, not clipped.
    let cancelled = false;
    void reactFlow.fitView({ nodes: [{ id: focusTarget }], padding: 0.2, minZoom: 0.2, maxZoom: 1 })
      .then(() => { if (!cancelled) setFocusTarget(null); });
    return () => { cancelled = true; };
  }, [focusTarget, nodesInitialized, viewportWidth, reactFlow]);

  // ─── Actions ─────────────────────────────────────────────────────────────

  const handleNodeDragStop = useCallback(
    (_: unknown, node: Node) => {
      saveLayout({ ...layout.positions, [node.id]: node.position });
    },
    [layout.positions, saveLayout],
  );

  const handleConnect = useCallback(
    (connection: Connection) => {
      if (!state.workflow || !connection.source || !connection.target) return;

      const targetStage = state.workflow.stages.find((s) => s.name === connection.target);
      const sourceStage = state.workflow.stages.find((s) => s.name === connection.source);
      if (!targetStage || !sourceStage) return;
      if (sourceStage.foreach || sourceStage.matrix) {
        setEditorError("Connect template outputs through Edit YAML using their expanded artifact paths.");
        return;
      }
      if (!sourceStage.outs.length) {
        setEditorError(`Declare an output written by ${sourceStage.name} before connecting it.`);
        return;
      }
      setEditorError(null);

      // Skip if dependency already exists
      const alreadyConnected = targetStage.deps.some(
        (d) => d.kind === "stage_out" && d.value.startsWith(`${connection.source}:`),
      );
      if (alreadyConnected) return;

      withHistory("Add connection", () => {
        const outputPath = artifactPath("crab.yaml", sourceStage.wdir, sourceStage.outs[0].path);
        actions.updateStage({
          ...targetStage,
          deps: [
            ...targetStage.deps,
            { kind: "stage_out" as const, value: `${connection.source}:${outputPath}` },
          ],
        });
      });
    },
    [state.workflow, actions, withHistory],
  );

  const handleDeleteEdge = useCallback(
    (edgeId: string) => {
      if (!state.workflow) return;
      const parsed = parseEdgeId(edgeId);
      if (!parsed) return;

      const targetStage = state.workflow.stages.find((s) => s.name === parsed.target);
      if (!targetStage) return;

      // Graph paths are local to this file here, just as in stageEdges().
      const localWorkflow = { ...state.workflow, path: "crab.yaml" };
      const linkedPaths = new Set(workflowLinks([localWorkflow])
        .filter(link => link.fromStage === parsed.source && link.toStage === parsed.target).map(link => link.path));
      withHistory("Remove connection", () => {
        const filteredDeps = targetStage.deps.filter(dep => {
          if (dep.kind === "stage_out") return !dep.value.startsWith(`${parsed.source}:`);
          return dep.kind !== "path" || !linkedPaths.has(artifactPath("crab.yaml", targetStage.wdir, dep.value));
        });
        actions.updateStage({ ...targetStage, deps: filteredDeps });
      });
      setSelectedEdge(null);
    },
    [state.workflow, actions, withHistory],
  );

  const handleDuplicateStage = useCallback(
    (name: string) => {
      if (!state.workflow) return;
      const original = state.workflow.stages.find((s) => s.name === name);
      if (!original) return;

      let newName = `${name}_copy`;
      let counter = 2;
      const existingNames = new Set(state.workflow.stages.map((s) => s.name));
      while (existingNames.has(newName)) {
        newName = `${name}_copy_${counter++}`;
      }

      withHistory("Duplicate stage", () => {
        actions.addStage({ ...original, name: newName, deps: [], outs: [] });
      });
      setSelectedStage(newName);
    },
    [state.workflow, actions, withHistory],
  );

  const handleAutoLayout = useCallback(() => {
    if (!state.workflow) return;
    fitViewDone.current = null;
    saveLayout(layoutStages(state.workflow.stages, {}, state.workflow.executions));
  }, [state.workflow, saveLayout, reactFlow]);

  const handleAddStage = useCallback(
    (stage: WorkflowStage) => {
      withHistory("Add stage", () => actions.addStage(stage));
      setShowNewDialog(false);
      setSelectedStage(stage.name);
    },
    [actions, withHistory],
  );

  const handleDeleteStage = useCallback(
    (name: string) => {
      withHistory("Delete stage", () => actions.removeStage(name));
      if (selectedStage === name) setSelectedStage(null);
    },
    [actions, selectedStage, withHistory],
  );

  // ─── Keyboard Shortcuts ────────────────────────────────────────────────────

  useEffect(() => {
    const handler = (e: KeyboardEvent) => {
      if (busy || state.phase === "running") return;
      const meta = e.metaKey || e.ctrlKey;
      const tag = (e.target as HTMLElement)?.tagName;
      if (tag === "INPUT" || tag === "TEXTAREA" || tag === "SELECT" || (e.target as HTMLElement)?.closest('aside[aria-label="Workflows"]')) return;

      // Delete/Backspace: remove selected edge or stage
      if (e.key === "Delete" || e.key === "Backspace") {
        if (selectedEdge) {
          e.preventDefault();
          handleDeleteEdge(selectedEdge);
        } else if (selectedStage) {
          e.preventDefault();
          handleDeleteStage(selectedStage);
        }
      }

      // Ctrl+Z: undo
      if (meta && e.key === "z" && !e.shiftKey) {
        e.preventDefault();
        if (state.workflow) {
          const prev = history.undo(state.workflow.stages);
          if (prev) actions.setStages(prev);
        }
      }

      // Ctrl+Shift+Z or Ctrl+Y: redo
      if (meta && (e.key === "Z" || e.key === "y")) {
        e.preventDefault();
        if (state.workflow) {
          const next = history.redo(state.workflow.stages);
          if (next) actions.setStages(next);
        }
      }

      // Ctrl+S: save
      if (meta && e.key === "s") {
        e.preventDefault();
        actions.save();
      }

      // Escape: deselect everything
      if (e.key === "Escape") {
        setSelectedStage(null);
        setSelectedEdge(null);
        setContextMenu(null);
      }

      // N: open new stage dialog
      if (e.key === "n" && !meta && !e.shiftKey && !e.altKey) {
        e.preventDefault();
        setShowNewDialog(true);
      }

      // Ctrl+D: duplicate selected stage
      if (meta && e.key === "d" && selectedStage) {
        e.preventDefault();
        handleDuplicateStage(selectedStage);
      }
    };

    window.addEventListener("keydown", handler);
    return () => window.removeEventListener("keydown", handler);
  }, [selectedStage, selectedEdge, state.workflow, state.phase, busy, actions, history, handleDeleteEdge, handleDuplicateStage, handleDeleteStage]);

  // ─── Event Handlers ────────────────────────────────────────────────────────

  const onNodeClick = useCallback((_: unknown, node: Node) => {
    setSelectedStage(node.id);
    setSelectedEdge(null);
    setContextMenu(null);
  }, []);

  const onEdgeClick = useCallback((_: unknown, edge: Edge) => {
    setSelectedEdge(edge.id);
    setSelectedStage(null);
    setContextMenu(null);
  }, []);

  const onPaneClick = useCallback(() => {
    setSelectedStage(null);
    setSelectedEdge(null);
    setContextMenu(null);
  }, []);

  const onNodeContextMenu = useCallback((event: React.MouseEvent, node: Node) => {
    event.preventDefault();
    if (locked) return;
    setContextMenu({ x: event.clientX, y: event.clientY, kind: "node", nodeId: node.id });
  }, [locked]);

  const onEdgeContextMenu = useCallback((event: React.MouseEvent, edge: Edge) => {
    event.preventDefault();
    if (locked) return;
    setContextMenu({ x: event.clientX, y: event.clientY, kind: "edge", edgeId: edge.id });
  }, [locked]);

  const onPaneContextMenu = useCallback((event: MouseEvent | React.MouseEvent) => {
    event.preventDefault();
    if (locked) return;
    setContextMenu({
      x: (event as React.MouseEvent).clientX,
      y: (event as React.MouseEvent).clientY,
      kind: "pane",
    });
  }, [locked]);

  // ─── Derived Values ────────────────────────────────────────────────────────

  const selectedStageData = useMemo(
    () => state.workflow?.stages.find((s) => s.name === selectedStage) ?? null,
    [state.workflow, selectedStage],
  );

  // Highlight selected edge with a different color
  const styledEdges = useMemo(
    () =>
      edges.map((edge) =>
        edge.id === selectedEdge
          ? { ...edge, style: { ...edge.style, stroke: "#4f8cff", strokeWidth: 3 }, animated: true }
          : edge,
      ),
    [edges, selectedEdge],
  );

  const workflows = useMemo(() => catalog.map(workflow => workflow.path === state.workflow?.path ? state.workflow : workflow), [catalog, state.workflow]);
  const links = useMemo(() => workflowLinks(workflows), [workflows]);
  const outputOptions = useMemo(() => availableWorkflowOutputs(workflows, state.activeFile ?? "", selectedStageData?.wdir), [workflows, state.activeFile, selectedStageData?.wdir]);
  const outdatedCount = statusReport?.stages.filter((s) => s.status === "outdated").length ?? 0;

  const openSource = async () => {
    if (!state.activeFile || !await actions.save()) return;
    try { setEditorError(null); await onOpenSource(state.activeFile); }
    catch (error) { setEditorError(String(error)); }
  };

  // ─── Main Render ───────────────────────────────────────────────────────────

  return (
    <div className="flex h-full w-full bg-gx-bg">
      <WorkflowSidebar files={state.availableFiles} workflows={workflows} fileErrors={fileErrors} activeFile={state.activeFile}
        links={links} disabled={locked} dirty={state.dirty} canRename={!!repo.path}
        onSelect={actions.switchFile} onRefresh={() => actions.load()} onCreate={actions.createWorkflow} onRename={actions.renameWorkflow} onDelete={actions.deleteWorkflow} onMigrate={() => setShowMigrateWizard(true)} />
      <div className="flex min-w-0 flex-1 flex-col">
      {(operationError || editorError) && <div role="alert" className="border-b border-gx-error/40 bg-gx-error/10 px-4 py-2 text-xs text-gx-error">{operationError || editorError}</div>}
      {state.phase === "loading" ? (
        <div role="status" className="flex flex-1 items-center justify-center gap-2 text-sm text-gx-muted"><Loader2 size={18} className="animate-spin" />Loading workflows…</div>
      ) : state.phase === "error" ? (
        <div className="flex flex-1 flex-col items-center justify-center gap-3 px-8">
          <AlertTriangle size={24} className="text-gx-error" />
          <p role="alert" aria-label="Workflow error" className="max-w-lg break-words text-sm text-gx-error">{state.error}</p>
          <button type="button" className="btn text-xs" onClick={() => actions.load(state.activeFile ?? undefined)}>Retry workflow</button>
          {state.activeFile && <button type="button" className="btn text-xs" onClick={openSource}>Edit YAML</button>}
        </div>
      ) : !state.workflow ? (
        <div className="flex flex-1 flex-col items-center justify-center gap-3 px-8 text-center">
          <FilePlus size={28} className="text-gx-muted" />
          <h2 className="text-base font-medium text-gx-text">Build your first workflow</h2>
          <p className="max-w-sm text-sm text-gx-muted">Add stages, connect their outputs, and run a repeatable pipeline for this repository.</p>
          <button type="button" className="btn btn-primary text-xs" disabled={busy} onClick={() => actions.createWorkflow("crab.yaml")}>Create Workflow</button>
        </div>
      ) : <>
      <div className="flex shrink-0 items-center justify-between gap-2 border-b border-gx-border bg-gx-surface pr-3">
        <WorkflowTabs active={activeTab} onChange={actions.setActiveTab} outdatedCount={outdatedCount} />
        <div className="flex shrink-0 items-center gap-2 py-1">
        {/* Run controls */}
        {state.phase === "running" ? (
          <button type="button" className="btn btn-danger flex items-center gap-1 text-xs" onClick={actions.cancel} disabled={cancelling}>
            <Square size={14} /> {cancelling ? "Stopping…" : "Cancel"}
          </button>
        ) : (
          <button type="button" className="btn btn-primary flex items-center gap-1 text-xs" onClick={() => actions.run()} disabled={busy || (state.phase !== "editing" && state.phase !== "done")}>
            <Play size={14} /> Run All
          </button>
        )}
        {state.phase === "done" && (
          <button type="button" className="btn text-xs" onClick={actions.resetRun}>Reset</button>
        )}
        </div>
      </div>

      {/* Toolbar */}
      <div className="flex shrink-0 flex-wrap items-center gap-2 border-b border-gx-border bg-gx-surface px-3 py-2">
        <div className="flex basis-full items-center gap-3">
        <div className="min-w-0 flex-1">
          <div className="truncate text-xs font-medium text-gx-text" title={state.activeFile ?? ""}>{state.activeFile}</div>
          <div role="status" className="mt-0.5 truncate text-[11px] text-gx-muted">{state.workflow.stages.length} {state.workflow.stages.length === 1 ? "stage" : "stages"}{state.workflow.executions && state.workflow.executions.length !== state.workflow.stages.length ? ` · ${state.workflow.executions.length} executions` : ""} · {state.dirty ? "Unsaved changes" : "Saved"}</div>
        </div>
        <button type="button" className="btn flex shrink-0 items-center gap-1 text-xs" disabled={locked} onClick={openSource}><Code2 size={14} />Edit YAML</button>
        </div>
        <select aria-label="Find stage" className="input-field min-w-40 flex-1 text-xs" value="" onChange={event => {
          const name = event.target.value;
          if (!name) return;
          setSelectedStage(name);
          setFocusTarget(name);
        }}>
          <option value="">Find stage…</option>
          {state.workflow.stages.map(stage => <option key={stage.name} value={stage.name}>{stage.name}</option>)}
        </select>
        <fieldset disabled={locked} className="flex items-center gap-2">
        {/* Add stage */}
        <button type="button" className="btn flex items-center gap-1 text-xs" onClick={() => setShowNewDialog(true)} title="Add Stage (N)">
          <Plus size={14} /> Stage
        </button>

        {/* Undo / Redo */}
        <button type="button" className={clsx("btn p-1.5", !history.canUndo && "opacity-40")} disabled={!history.canUndo} title="Undo (Ctrl+Z)" onClick={() => { if (state.workflow) { const prev = history.undo(state.workflow.stages); if (prev) actions.setStages(prev); } }}>
          <Undo2 size={14} />
        </button>
        <button type="button" className={clsx("btn p-1.5", !history.canRedo && "opacity-40")} disabled={!history.canRedo} title="Redo (Ctrl+Shift+Z)" onClick={() => { if (state.workflow) { const next = history.redo(state.workflow.stages); if (next) actions.setStages(next); } }}>
          <Redo2 size={14} />
        </button>

        {/* Auto-layout */}
        <button type="button" className="btn flex items-center gap-1 text-xs" onClick={handleAutoLayout} title="Arrange stages">
          <LayoutGrid size={14} /> Arrange
        </button>

        {/* Save */}
        <button type="button" className={clsx("btn flex items-center gap-1 text-xs", state.dirty && "text-gx-accent")} onClick={actions.save} disabled={!state.dirty} title="Save (Ctrl+S)">
          <Save size={14} />
          {state.dirty && <span className="h-1.5 w-1.5 rounded-full bg-gx-accent" />}
        </button>

        <button type="button" className="btn p-1.5" onClick={() => reactFlow.fitView({ padding: 0.2, maxZoom: 1 })} aria-label="Fit workflow to view"><Maximize2 size={14} /></button>
        </fieldset>

        {/* Legend toggle */}
        <button type="button" className={clsx("btn p-1.5", showLegend && "text-gx-accent")} onClick={() => setShowLegend(!showLegend)} title="Legend / help">
          <HelpCircle size={14} />
        </button>

        {/* Remove edge button (visible when edge selected) */}
        {selectedEdge && (
          <button type="button" className="btn flex items-center gap-1 text-xs text-gx-error/70 hover:text-gx-error" disabled={locked} onClick={() => handleDeleteEdge(selectedEdge)} title="Remove connection (Delete)">
            <Trash2 size={14} /> Remove Edge
          </button>
        )}


      </div>

      {/* Editor tab */}
      {activeTab === "editor" && (
        <div className="flex min-h-0 flex-1 flex-col">
          <div className="flex min-h-0 flex-1">
            {/* Canvas */}
            <div ref={canvasRef} className="min-w-0 flex-1">
              <ReactFlow
                nodes={nodes}
                edges={styledEdges}
                onNodesChange={onNodesChange}
                onEdgesChange={onEdgesChange}
                onConnect={handleConnect}
                onNodeClick={onNodeClick}
                onEdgeClick={onEdgeClick}
                onNodeDragStop={handleNodeDragStop}
                onPaneClick={onPaneClick}
                onNodeContextMenu={onNodeContextMenu}
                onEdgeContextMenu={onEdgeContextMenu}
                onPaneContextMenu={onPaneContextMenu}
                nodeTypes={nodeTypes}
                nodesDraggable={!locked}
                nodesConnectable={!locked}
                minZoom={0.02}
                maxZoom={1.5}
                fitViewOptions={fitViewOptions}
                fitView
                deleteKeyCode={null}
                proOptions={{ hideAttribution: true }}
                className="bg-gx-bg"
              >
                <Background color="rgb(var(--gx-border))" gap={24} size={1} />
                <Controls className="!border-gx-border !bg-gx-surface [&_button]:!border-gx-border [&_button]:!bg-gx-surface [&_button]:!fill-gx-muted hover:[&_button]:!bg-gx-surface-hover" />
                {nodes.length > 8 && <MiniMap className="!border-gx-border !bg-gx-surface" nodeColor="rgb(var(--gx-accent))" />}

                {state.runState && (
                  <Panel position="top-right"><RunStatusBadge runState={state.runState} /></Panel>
                )}
                {showLegend && (
                  <Panel position="top-left"><LegendPanel /></Panel>
                )}
                {state.workflow && state.workflow.stages.length === 0 && (
                  <Panel position="top-center">
                    <div className="rounded border border-gx-border bg-gx-surface px-4 py-3 text-center text-xs text-gx-muted shadow">
                      <div className="mb-1 font-medium text-gx-text">Empty pipeline</div>
                      Click &quot;+ Stage&quot; or press <kbd className="rounded border border-gx-border bg-gx-bg px-1 py-0.5 text-[10px]">N</kbd> to add your first stage.
                    </div>
                  </Panel>
                )}
              </ReactFlow>
            </div>

            {/* Properties panel */}
            {selectedStageData && (
              <fieldset disabled={locked} className="flex min-h-0 shrink-0">
              <StagePropertiesPanel
                stage={selectedStageData}
                workflowOutputs={outputOptions}
                runStatus={state.runState?.stages.find((s) => s.name === selectedStage) ?? null}
                onUpdate={(stage) => { withHistory("Update stage", () => actions.updateStage(stage)); }}
                onRename={(oldName, newName) => { withHistory("Rename", () => actions.renameStage(oldName, newName)); setSelectedStage(newName); }}
                onDelete={() => handleDeleteStage(selectedStage!)}
                onClose={() => setSelectedStage(null)}
                onRun={() => actions.run([selectedStage!])}
                isRunning={state.phase === "running"}
              />
              </fieldset>
            )}
          </div>

          {/* Output pane */}
          <WorkflowOutputPane runState={state.runState} onClear={actions.clearOutput} />
        </div>
      )}

      {/* Status tab */}
      {activeTab === "status" && (
        <div className="min-h-0 flex-1">
          <StatusDashboard repoId={repo.repo_id} workflowPath={state.activeFile} onRunOutdated={actions.runOutdated} onRunAll={() => actions.run()} />
        </div>
      )}

      {/* Experiments tab */}
      {activeTab === "experiments" && (
        <div className="min-h-0 flex-1">
          <ExperimentPanel repoId={repo.repo_id} />
        </div>
      )}

      </>}
      </div>

      {/* Context menu */}
      {contextMenu && (
        <ContextMenuPopup
          menu={contextMenu}
          onClose={() => setContextMenu(null)}
          onDeleteEdge={handleDeleteEdge}
          onDeleteStage={handleDeleteStage}
          onDuplicate={handleDuplicateStage}
          onRun={(name) => actions.run([name])}
          onFreeze={(name) => {
            const stage = state.workflow?.stages.find((s) => s.name === name);
            if (stage) {
              withHistory("Toggle freeze", () => actions.updateStage({ ...stage, frozen: !stage.frozen }));
            }
          }}
          onAddStage={() => setShowNewDialog(true)}
          onAutoLayout={handleAutoLayout}
        />
      )}

      {/* Dialogs */}
      {showNewDialog && (
        <NewStageDialog
          existingNames={state.workflow?.stages.map((s) => s.name) ?? []}
          onAdd={handleAddStage}
          onClose={() => setShowNewDialog(false)}
        />
      )}
      {showMigrateWizard && (
        <MigrateWizard
          repoId={repo.repo_id}
          onComplete={(path) => { setShowMigrateWizard(false); actions.switchFile(path); }}
          onClose={() => setShowMigrateWizard(false)}
        />
      )}
    </div>
  );
}

// ─── Context Menu Popup ──────────────────────────────────────────────────────

function ContextMenuPopup({
  menu,
  onClose,
  onDeleteEdge,
  onDeleteStage,
  onDuplicate,
  onRun,
  onFreeze,
  onAddStage,
  onAutoLayout,
}: {
  menu: ContextMenu;
  onClose: () => void;
  onDeleteEdge: (id: string) => void;
  onDeleteStage: (name: string) => void;
  onDuplicate: (name: string) => void;
  onRun: (name: string) => void;
  onFreeze: (name: string) => void;
  onAddStage: () => void;
  onAutoLayout: () => void;
}) {
  const ref = useRef<HTMLDivElement>(null);

  useEffect(() => {
    const handler = (e: MouseEvent) => {
      if (ref.current && !ref.current.contains(e.target as HTMLElement)) {
        onClose();
      }
    };
    document.addEventListener("mousedown", handler);
    return () => document.removeEventListener("mousedown", handler);
  }, [onClose]);

  const items: { label: string; icon: React.ReactNode; action: () => void; danger?: boolean }[] = [];

  if (menu.kind === "node" && menu.nodeId) {
    const name = menu.nodeId;
    items.push({ label: "Run this stage", icon: <Play size={12} />, action: () => { onRun(name); onClose(); } });
    items.push({ label: "Duplicate", icon: <Copy size={12} />, action: () => { onDuplicate(name); onClose(); } });
    items.push({ label: "Freeze / Unfreeze", icon: <Snowflake size={12} />, action: () => { onFreeze(name); onClose(); } });
    items.push({ label: "Delete stage", icon: <Trash2 size={12} />, action: () => { onDeleteStage(name); onClose(); }, danger: true });
  } else if (menu.kind === "edge" && menu.edgeId) {
    items.push({ label: "Remove connection", icon: <Trash2 size={12} />, action: () => { onDeleteEdge(menu.edgeId!); onClose(); }, danger: true });
  } else {
    items.push({ label: "Add stage here", icon: <Plus size={12} />, action: () => { onAddStage(); onClose(); } });
    items.push({ label: "Auto-layout", icon: <LayoutGrid size={12} />, action: () => { onAutoLayout(); onClose(); } });
  }

  return (
    <div
      ref={ref}
      className="fixed z-[100] min-w-[160px] rounded border border-gx-border bg-gx-surface py-1 shadow-xl"
      style={{ left: menu.x, top: menu.y }}
    >
      {items.map((item, i) => (
        <button
          key={i}
          type="button"
          className={clsx(
            "flex w-full items-center gap-2 px-3 py-1.5 text-left text-xs hover:bg-gx-surface-hover",
            item.danger ? "text-gx-error" : "text-gx-text",
          )}
          onClick={item.action}
        >
          {item.icon}
          {item.label}
        </button>
      ))}
    </div>
  );
}

// ─── Legend Panel ─────────────────────────────────────────────────────────────

function LegendPanel() {
  return (
    <div className="rounded border border-gx-border bg-gx-surface p-3 text-[10px] shadow-lg">
      <div className="mb-2 text-xs font-medium text-gx-text">Legend &amp; Shortcuts</div>
      <div className="space-y-1.5 text-gx-muted">
        <div className="flex items-center gap-2"><span className="inline-block h-2 w-2 rounded-full bg-gx-muted" /> Pending</div>
        <div className="flex items-center gap-2"><span className="inline-block h-2 w-2 animate-pulse rounded-full bg-gx-info" /> Running</div>
        <div className="flex items-center gap-2"><span className="inline-block h-2 w-2 rounded-full bg-gx-success" /> Done</div>
        <div className="flex items-center gap-2"><span className="inline-block h-2 w-2 rounded-full bg-gx-error" /> Failed</div>
        <div className="flex items-center gap-2"><span className="inline-block h-2 w-2 rounded-full bg-gx-thinking" /> Cached</div>
        <hr className="border-gx-border" />
        <div className="flex items-center gap-2"><span className="inline-block h-0.5 w-4 bg-gray-500" /> Dependency</div>
        <div className="flex items-center gap-2"><span className="inline-block h-0.5 w-4 bg-blue-500" /> Selected</div>
        <div className="flex items-center gap-2"><span className="inline-block h-0.5 w-4 bg-red-500" /> Cycle</div>
        <hr className="border-gx-border" />
        <div><kbd className="rounded border border-gx-border bg-gx-bg px-1">N</kbd> Add stage</div>
        <div><kbd className="rounded border border-gx-border bg-gx-bg px-1">Del</kbd> Delete selection</div>
        <div><kbd className="rounded border border-gx-border bg-gx-bg px-1">Ctrl+Z</kbd> Undo</div>
        <div><kbd className="rounded border border-gx-border bg-gx-bg px-1">Ctrl+Shift+Z</kbd> Redo</div>
        <div><kbd className="rounded border border-gx-border bg-gx-bg px-1">Ctrl+D</kbd> Duplicate</div>
        <div><kbd className="rounded border border-gx-border bg-gx-bg px-1">Ctrl+S</kbd> Save</div>
        <div><kbd className="rounded border border-gx-border bg-gx-bg px-1">Esc</kbd> Deselect</div>
        <hr className="border-gx-border" />
        <div>Right-click nodes/edges for actions</div>
        <div>Click edge to select, then Del to remove</div>
        <div>Drag bottom handle to top handle to connect</div>
      </div>
    </div>
  );
}

// ─── Workflow Output Pane ────────────────────────────────────────────────────

function WorkflowOutputPane({ runState, onClear }: { runState: WorkflowRunState | null; onClear: () => void }) {
  const lines = runState?.output ?? [];
  const [collapsed, setCollapsed] = useState(!runState);
  const scrollRef = useRef<HTMLDivElement>(null);

  useEffect(() => { setCollapsed(!runState); }, [runState?.run_id, runState?.error]);

  useEffect(() => {
    if (scrollRef.current) {
      scrollRef.current.scrollTop = scrollRef.current.scrollHeight;
    }
  }, [lines, runState?.error]);

  return (
    <div role="region" aria-label="Workflow output" className={clsx("shrink-0 border-t border-gx-border bg-gx-surface", collapsed ? "h-7" : "h-[160px]")}>
      <div className="flex h-7 items-center justify-between border-b border-gx-border px-3">
        <button
          type="button"
          className="flex items-center gap-1.5 text-[11px] font-medium text-gx-muted hover:text-gx-text"
          onClick={() => setCollapsed(!collapsed)}
          aria-expanded={!collapsed}
        >
          <ChevronDown size={12} className={clsx("transition", collapsed && "-rotate-90")} />
          Output
          {lines.length > 0 && (
            <span className="rounded bg-gx-accent/15 px-1.5 py-0.5 text-[9px] font-medium text-gx-accent">
              {lines.length}
            </span>
          )}
        </button>
        {!collapsed && lines.length > 0 && (
          <button type="button" className="text-[10px] text-gx-muted hover:text-gx-text" onClick={onClear}>
            Clear
          </button>
        )}
      </div>
      {!collapsed && (
        <div ref={scrollRef} className="h-[calc(100%-28px)] overflow-auto px-3 py-1 font-mono text-[11px]">
          {lines.length === 0 && !runState?.error ? (
            <div className="py-4 text-center text-gx-muted">{runState?.status === "running" ? "Waiting for workflow output…" : runState ? "No command output for this run." : "Run a workflow to see output here."}</div>
          ) : (
            lines.map((line) => (
              <div
                key={line.id}
                className={clsx("whitespace-pre-wrap break-all leading-relaxed", line.error ? "text-gx-error" : "text-gx-text/80")}
              >
                {line.text}
              </div>
            ))
          )}
          {runState?.error && <div role="alert" className="whitespace-pre-wrap break-words py-1 text-gx-error">{runState.error}</div>}
        </div>
      )}
    </div>
  );
}

// ─── Run Status Badge ────────────────────────────────────────────────────────

function RunStatusBadge({ runState }: { runState: import("../lib/workflow-types").WorkflowRunState }) {
  const done = runState.stages.filter((s) => s.status === "done" || s.status === "cached").length;
  const failed = runState.stages.filter((s) => s.status === "failed").length;
  const total = runState.stages.length;
  const skipped = runState.stages.filter(stage => stage.status === "skipped").length;

  return (
    <div className="rounded border border-gx-border bg-gx-surface px-3 py-2 text-xs shadow-lg">
      <div className="mb-1 font-medium text-gx-text">
        {runState.status === "running" && "Running..."}
        {runState.status === "done" && "Completed"}
        {runState.status === "failed" && "Failed"}
        {runState.status === "cancelled" && "Cancelled"}
      </div>
      <div className="flex gap-3 text-gx-muted">
        <span className="text-gx-success">{done} done</span>
        {failed > 0 && <span className="text-gx-error">{failed} failed</span>}
        {skipped > 0 && <span>{skipped} skipped</span>}
        <span>{total} total</span>
      </div>
      {runState.status === "running" && (
        <div className="mt-1.5 h-1 w-full overflow-hidden rounded-full bg-gx-bg">
          <div
            className="h-full rounded-full bg-gx-accent transition-all"
            style={{ width: `${((done + failed + skipped) / Math.max(1, total)) * 100}%` }}
          />
        </div>
      )}
    </div>
  );
}

// ─── Export ──────────────────────────────────────────────────────────────────

export function WorkflowEditorView({ repo, onOpenSource }: Props) {
  return (
    <ReactFlowProvider key={repo.repo_id}>
      <WorkflowEditorInner repo={repo} onOpenSource={onOpenSource} />
    </ReactFlowProvider>
  );
}
