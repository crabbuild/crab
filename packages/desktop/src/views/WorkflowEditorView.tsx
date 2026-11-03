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
  ReactFlowProvider,
} from "@xyflow/react";
import clsx from "clsx";
import {
  AlertTriangle,
  ChevronDown,
  Copy,
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
  ArrowRightLeft,
  SendToBack,
} from "lucide-react";
import { OpenRepo } from "../state";
import { mainProcess, onAgentEvent } from "../ipc";
import { useWorkflowEditor } from "../hooks/useWorkflowEditor";
import { WorkflowStage, StageRunStatus } from "../lib/workflow-types";
import { StageNode } from "../components/workflow/StageNode";
import { StagePropertiesPanel } from "../components/workflow/StagePropertiesPanel";
import { NewStageDialog } from "../components/workflow/NewStageDialog";
import { WorkflowTabs } from "../components/workflow/WorkflowTabs";
import { StatusDashboard } from "../components/workflow/StatusDashboard";
import { ExperimentPanel } from "../components/workflow/ExperimentPanel";
import { MigrateWizard } from "../components/workflow/MigrateWizard";

// ─── Types ───────────────────────────────────────────────────────────────────

interface Props {
  repo: OpenRepo;
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
    canUndo: past.length > 0,
    canRedo: future.length > 0,
  };
}

// ─── Utility: Cycle Detection ────────────────────────────────────────────────

/**
 * Detects cycles in the stage dependency graph using DFS coloring.
 * Returns the set of stage names that participate in at least one cycle.
 */
function detectCycles(stages: WorkflowStage[]): Set<string> {
  const stageNames = new Set(stages.map((stage) => stage.name));

  // Build adjacency list: parent -> children (stages that depend on parent)
  const adjacency = new Map<string, string[]>();
  for (const stage of stages) {
    adjacency.set(stage.name, []);
  }

  for (const stage of stages) {
    for (const dep of stage.deps) {
      let parentName: string | null = null;

      if (dep.kind === "stage_out") {
        parentName = dep.value.split(":")[0];
      } else if (dep.kind === "path") {
        const colonIndex = dep.value.indexOf(":");
        if (colonIndex > 0) {
          const maybeStageName = dep.value.slice(0, colonIndex);
          if (stageNames.has(maybeStageName)) {
            parentName = maybeStageName;
          }
        }
      }

      if (parentName && stageNames.has(parentName)) {
        adjacency.get(parentName)!.push(stage.name);
      }
    }
  }

  // DFS with 3-color marking to find back edges (cycles)
  const cycleMembers = new Set<string>();
  const WHITE = 0;
  const GRAY = 1;
  const BLACK = 2;
  const color = new Map<string, number>();

  for (const name of stageNames) {
    color.set(name, WHITE);
  }

  function dfs(node: string, path: string[]) {
    color.set(node, GRAY);
    path.push(node);

    for (const neighbor of adjacency.get(node)!) {
      if (color.get(neighbor) === GRAY) {
        // Back edge found — mark all nodes in the cycle
        const cycleStart = path.indexOf(neighbor);
        for (let i = cycleStart; i < path.length; i++) {
          cycleMembers.add(path[i]);
        }
      } else if (color.get(neighbor) === WHITE) {
        dfs(neighbor, path);
      }
    }

    path.pop();
    color.set(node, BLACK);
  }

  for (const name of stageNames) {
    if (color.get(name) === WHITE) {
      dfs(name, []);
    }
  }

  return cycleMembers;
}

// ─── Utility: Auto-Layout (Topological Sort) ─────────────────────────────────

/**
 * Computes node positions using a topological sort (Kahn's algorithm).
 * Nodes are arranged in horizontal layers by depth. If `force` is false,
 * existing positions from `currentPositions` are preserved.
 */
function autoLayout(
  stages: WorkflowStage[],
  currentPositions: Record<string, { x: number; y: number }>,
  force = false,
): Record<string, { x: number; y: number }> {
  const result: Record<string, { x: number; y: number }> = {};

  const NODE_WIDTH = 100;
  const NODE_HEIGHT = 32;
  const HORIZONTAL_GAP = 24;
  const VERTICAL_GAP = 40;

  const stageNames = new Set(stages.map((stage) => stage.name));

  // Build in-degree map and children adjacency
  const inDegree = new Map<string, number>();
  const children = new Map<string, string[]>();

  for (const stage of stages) {
    inDegree.set(stage.name, 0);
    children.set(stage.name, []);
  }

  for (const stage of stages) {
    for (const dep of stage.deps) {
      let parentName: string | null = null;

      if (dep.kind === "stage_out") {
        parentName = dep.value.split(":")[0];
      } else if (dep.kind === "path") {
        const colonIndex = dep.value.indexOf(":");
        if (colonIndex > 0) {
          const maybeStageName = dep.value.slice(0, colonIndex);
          if (stageNames.has(maybeStageName)) {
            parentName = maybeStageName;
          }
        }
      }

      if (parentName && stageNames.has(parentName)) {
        children.get(parentName)!.push(stage.name);
        inDegree.set(stage.name, (inDegree.get(stage.name) ?? 0) + 1);
      }
    }
  }

  // Kahn's algorithm: compute depth level for each node
  const level = new Map<string, number>();
  const queue: string[] = [];

  for (const [name, degree] of inDegree) {
    if (degree === 0) {
      queue.push(name);
    }
  }
  queue.sort();

  while (queue.length) {
    const current = queue.shift()!;
    const currentLevel = level.get(current) ?? 0;
    level.set(current, currentLevel);

    for (const child of children.get(current)!) {
      const newDegree = (inDegree.get(child) ?? 1) - 1;
      inDegree.set(child, newDegree);
      level.set(child, Math.max(level.get(child) ?? 0, currentLevel + 1));
      if (newDegree === 0) {
        queue.push(child);
      }
    }
    queue.sort();
  }

  // Group nodes by their level
  const nodesByLevel = new Map<number, string[]>();
  for (const [name, lvl] of level) {
    if (!nodesByLevel.has(lvl)) {
      nodesByLevel.set(lvl, []);
    }
    nodesByLevel.get(lvl)!.push(name);
  }

  // Handle orphan nodes (not reachable via topological sort, e.g. in cycles)
  for (const stage of stages) {
    if (!level.has(stage.name)) {
      const maxLevel = Math.max(0, ...nodesByLevel.keys()) + 1;
      if (!nodesByLevel.has(maxLevel)) {
        nodesByLevel.set(maxLevel, []);
      }
      nodesByLevel.get(maxLevel)!.push(stage.name);
    }
  }

  // Assign positions: center each level horizontally
  for (const [, nodesInLevel] of nodesByLevel) {
    nodesInLevel.sort();
    const totalWidth =
      nodesInLevel.length * NODE_WIDTH + (nodesInLevel.length - 1) * HORIZONTAL_GAP;
    const startX = -totalWidth / 2;

    for (let i = 0; i < nodesInLevel.length; i++) {
      const name = nodesInLevel[i];
      if (!force && currentPositions[name]) {
        result[name] = currentPositions[name];
      } else {
        result[name] = {
          x: startX + i * (NODE_WIDTH + HORIZONTAL_GAP),
          y: (level.get(name) ?? 0) * (NODE_HEIGHT + VERTICAL_GAP),
        };
      }
    }
  }

  return result;
}

// ─── Utility: Edge ID Parsing ────────────────────────────────────────────────

/** Parses an edge ID like "source->target" or "source->target:extra" into source/target. */
function parseEdgeId(id: string): { source: string; target: string } | null {
  const match = id.match(/^(.+)->(.+?)(?::.*)?$/);
  return match ? { source: match[1], target: match[2] } : null;
}

// ─── Main Component ──────────────────────────────────────────────────────────

function WorkflowEditorInner({ repo }: Props) {
  const { state, layout, saveLayout, actions, statusReport, activeTab } =
    useWorkflowEditor(repo.repo_id);

  // ReactFlow state
  const [nodes, setNodes, onNodesChange] = useNodesState<Node>([]);
  const [edges, setEdges, onEdgesChange] = useEdgesState<Edge>([]);

  // Selection state
  const [selectedStage, setSelectedStage] = useState<string | null>(null);
  const [selectedEdge, setSelectedEdge] = useState<string | null>(null);

  // Dialog visibility
  const [showNewDialog, setShowNewDialog] = useState(false);
  const [showFileMenu, setShowFileMenu] = useState(false);
  const [showMigrateWizard, setShowMigrateWizard] = useState(false);
  const [showLegend, setShowLegend] = useState(false);

  // Context menu
  const [contextMenu, setContextMenu] = useState<ContextMenu | null>(null);

  // Refs and hooks
  const reactFlow = useReactFlow();
  const fitViewDone = useRef(false);
  const history = useUndoRedo();

  // Derived state
  const cycleNodes = useMemo(
    () => (state.workflow ? detectCycles(state.workflow.stages) : new Set<string>()),
    [state.workflow],
  );

  // Helper: push history before a mutation
  const withHistory = useCallback(
    (label: string, mutation: () => void) => {
      if (state.workflow) {
        history.push(state.workflow.stages, label);
      }
      mutation();
    },
    [state.workflow, history],
  );

  // ─── Graph Building ──────────────────────────────────────────────────────

  const buildGraph = useCallback(() => {
    if (!state.workflow) {
      setNodes([]);
      setEdges([]);
      return;
    }

    const stages = state.workflow.stages;
    const positions = autoLayout(stages, layout.positions);

    // Build run status lookup
    const statusMap = new Map<string, StageRunStatus>();
    if (state.runState) {
      for (const stageRun of state.runState.stages) {
        statusMap.set(stageRun.name, stageRun.status);
      }
    }

    const stageNames = new Set(stages.map((s) => s.name));

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
        },
      })),
    );

    // Create edges from dependencies
    const newEdges: Edge[] = [];

    for (const stage of stages) {
      for (const dep of stage.deps) {
        if (dep.kind === "stage_out") {
          const parentName = dep.value.split(":")[0];
          if (!stageNames.has(parentName)) continue;

          const isCycleEdge = cycleNodes.has(parentName) && cycleNodes.has(stage.name);
          newEdges.push({
            id: `${parentName}->${stage.name}`,
            source: parentName,
            target: stage.name,
            animated: statusMap.get(parentName) === "running",
            markerEnd: {
              type: MarkerType.ArrowClosed,
              color: isCycleEdge ? "#ef4444" : "#6b7280",
            },
            style: {
              stroke: isCycleEdge ? "#ef4444" : "#6b7280",
              strokeWidth: 2,
            },
            label: isCycleEdge ? "cycle!" : undefined,
            labelStyle: isCycleEdge
              ? { fontSize: 9, fill: "#ef4444", fontWeight: 600 }
              : undefined,
          });
        } else if (dep.kind === "path") {
          const colonIdx = dep.value.indexOf(":");
          if (colonIdx > 0) {
            const maybeStageName = dep.value.slice(0, colonIdx);
            if (stageNames.has(maybeStageName)) {
              newEdges.push({
                id: `${maybeStageName}->${stage.name}`,
                source: maybeStageName,
                target: stage.name,
                animated: statusMap.get(maybeStageName) === "running",
                markerEnd: { type: MarkerType.ArrowClosed, color: "#6b7280" },
                style: { stroke: "#6b7280", strokeWidth: 2 },
              });
              continue;
            }
          }
          // Implicit file-based edges
          for (const other of stages) {
            if (other.name === stage.name) continue;
            for (const out of other.outs) {
              if (out.path === dep.value) {
                newEdges.push({
                  id: `${other.name}->${stage.name}:${dep.value}`,
                  source: other.name,
                  target: stage.name,
                  animated: statusMap.get(other.name) === "running",
                  markerEnd: { type: MarkerType.ArrowClosed, color: "#6b7280" },
                  style: { stroke: "#6b7280", strokeWidth: 2, strokeDasharray: "5 5" },
                });
              }
            }
          }
        }
      }
    }

    // Visual-only edges (persisted layout hints)
    const visualEdges: Edge[] = (layout.visualEdges ?? [])
      .filter((ve) => stageNames.has(ve.source) && stageNames.has(ve.target))
      .map((ve) => ({
        id: `${ve.source}->${ve.target}:vis`,
        source: ve.source,
        target: ve.target,
        markerEnd: { type: MarkerType.ArrowClosed, color: "#6b7280" },
        style: { stroke: "#6b7280", strokeWidth: 2, strokeDasharray: "5 5" },
      }));

    setEdges([...newEdges, ...visualEdges]);
  }, [state.workflow, state.runState, layout.positions, layout.visualEdges, selectedStage, cycleNodes, setNodes, setEdges]);

  useEffect(() => {
    buildGraph();
  }, [buildGraph]);

  // Fit view on first load
  useEffect(() => {
    if (state.phase === "editing" && !fitViewDone.current && nodes.length > 0) {
      fitViewDone.current = true;
      setTimeout(() => reactFlow.fitView({ padding: 0.3 }), 100);
    }
  }, [state.phase, nodes.length, reactFlow]);

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

      // Skip if dependency already exists
      const alreadyConnected = targetStage.deps.some(
        (d) => d.kind === "stage_out" && d.value.startsWith(`${connection.source}:`),
      );
      if (alreadyConnected) return;

      withHistory("Add connection", () => {
        let outputPath: string;
        if (sourceStage.outs.length > 0) {
          outputPath = sourceStage.outs[0].path;
        } else {
          outputPath = `${connection.source}.out`;
          actions.updateStage({
            ...sourceStage,
            outs: [...sourceStage.outs, { path: outputPath, kind: "file" }],
          });
        }
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

      withHistory("Remove connection", () => {
        const filteredDeps = targetStage.deps.filter((dep) => {
          if (dep.kind === "stage_out" && dep.value.startsWith(`${parsed.source}:`)) {
            return false;
          }
          if (dep.kind === "path") {
            const colonIdx = dep.value.indexOf(":");
            if (colonIdx > 0 && dep.value.slice(0, colonIdx) === parsed.source) {
              return false;
            }
          }
          return true;
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
    saveLayout(autoLayout(state.workflow.stages, {}, true));
    setTimeout(() => reactFlow.fitView({ padding: 0.3 }), 50);
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
      const meta = e.metaKey || e.ctrlKey;
      const tag = (e.target as HTMLElement)?.tagName;
      if (tag === "INPUT" || tag === "TEXTAREA" || tag === "SELECT") return;

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
  }, [selectedStage, selectedEdge, state.workflow, actions, history, handleDeleteEdge, handleDuplicateStage, handleDeleteStage]);

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
    setContextMenu({ x: event.clientX, y: event.clientY, kind: "node", nodeId: node.id });
  }, []);

  const onEdgeContextMenu = useCallback((event: React.MouseEvent, edge: Edge) => {
    event.preventDefault();
    setContextMenu({ x: event.clientX, y: event.clientY, kind: "edge", edgeId: edge.id });
  }, []);

  const onPaneContextMenu = useCallback((event: MouseEvent | React.MouseEvent) => {
    event.preventDefault();
    setContextMenu({
      x: (event as React.MouseEvent).clientX,
      y: (event as React.MouseEvent).clientY,
      kind: "pane",
    });
  }, []);

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

  // ─── Early Returns (Loading / Empty / Error) ───────────────────────────────

  if (state.phase === "loading") {
    return (
      <div className="flex h-full items-center justify-center bg-gx-bg">
        <Loader2 className="animate-spin text-gx-muted" size={24} />
      </div>
    );
  }

  if (state.phase === "empty") {
    return (
      <div className="flex h-full flex-col items-center justify-center gap-6 bg-gx-bg px-8">
        <div className="flex flex-col items-center gap-3 text-center">
          <div className="flex h-16 w-16 items-center justify-center rounded-full bg-gx-accent/10">
            <FilePlus size={28} className="text-gx-accent" />
          </div>
          <div className="text-lg font-medium text-gx-text">No Workflow Found</div>
          <div className="max-w-sm text-sm text-gx-muted">
            A workflow defines a pipeline of stages with dependencies. Create a{" "}
            <code className="rounded bg-gx-surface px-1 py-0.5 text-xs">crab.yaml</code> to get
            started.
          </div>
        </div>
        <button
          type="button"
          className="btn btn-primary flex items-center gap-2"
          onClick={() => actions.createWorkflow("crab.yaml")}
        >
          <FilePlus size={14} /> Create Workflow
        </button>
        <div className="mt-4 max-w-md rounded border border-gx-border bg-gx-surface p-4 text-xs text-gx-muted">
          <div className="mb-2 font-medium text-gx-text">Quick start:</div>
          <ol className="list-inside list-decimal space-y-1">
            <li>Create a workflow file</li>
            <li>Add stages (each runs a shell command)</li>
            <li>Connect stages by dragging between nodes</li>
            <li>Click Run All to execute the pipeline</li>
          </ol>
        </div>
      </div>
    );
  }

  if (state.phase === "error") {
    return (
      <div className="flex h-full flex-col items-center justify-center gap-4 bg-gx-bg">
        <AlertTriangle className="text-gx-error" size={32} />
        <div className="max-w-md text-center text-sm text-gx-error">
          {typeof state.error === "string" ? state.error : JSON.stringify(state.error)}
        </div>
        <button type="button" className="btn" onClick={() => actions.load()}>
          Retry
        </button>
      </div>
    );
  }

  const outdatedCount = statusReport?.stages.filter((s) => s.status === "outdated").length ?? 0;

  // ─── Main Render ───────────────────────────────────────────────────────────

  return (
    <div className="flex h-full w-full flex-col bg-gx-bg">
      <WorkflowTabs active={activeTab} onChange={actions.setActiveTab} outdatedCount={outdatedCount} />

      {/* Toolbar */}
      <div className="flex shrink-0 items-center gap-2 border-b border-gx-border bg-gx-surface px-3 py-2">
        {/* File selector */}
        <div className="relative">
          <button
            type="button"
            className="btn flex items-center gap-1 text-xs"
            onClick={() => setShowFileMenu(!showFileMenu)}
          >
            <span className="max-w-[160px] truncate font-mono">{state.activeFile}</span>
            <ChevronDown size={12} />
          </button>
          {showFileMenu && (
            <div className="absolute left-0 top-full z-50 mt-1 min-w-[200px] rounded border border-gx-border bg-gx-surface shadow-lg">
              {state.availableFiles.map((f) => (
                <button
                  key={f}
                  type="button"
                  className={clsx(
                    "block w-full px-3 py-1.5 text-left text-xs hover:bg-gx-surface-hover",
                    f === state.activeFile && "bg-gx-accent/10 text-gx-accent",
                  )}
                  onClick={() => { actions.switchFile(f); setShowFileMenu(false); }}
                >
                  {f}
                </button>
              ))}
              <div className="border-t border-gx-border">
                <button
                  type="button"
                  className="flex w-full items-center gap-1 px-3 py-1.5 text-left text-xs text-gx-muted hover:bg-gx-surface-hover"
                  onClick={async () => {
                    const name = await mainProcess.dialogPrompt("Workflow file name:", "pipeline.yaml");
                    if (name) actions.createWorkflow(name);
                    setShowFileMenu(false);
                  }}
                >
                  <Plus size={12} /> New workflow
                </button>
              </div>
            </div>
          )}
        </div>

        <div className="mx-2 h-4 w-px bg-gx-border" />

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
        <button type="button" className="btn p-1.5" onClick={handleAutoLayout} title="Auto-layout">
          <SendToBack size={14} />
        </button>

        {/* Save */}
        <button type="button" className={clsx("btn flex items-center gap-1 text-xs", state.dirty && "text-gx-accent")} onClick={actions.save} disabled={!state.dirty} title="Save (Ctrl+S)">
          <Save size={14} />
          {state.dirty && <span className="h-1.5 w-1.5 rounded-full bg-gx-accent" />}
        </button>

        {/* Migrate */}
        <button type="button" className="btn flex items-center gap-1 text-xs" onClick={() => setShowMigrateWizard(true)} title="Migrate from DVC">
          <ArrowRightLeft size={14} /> Migrate
        </button>

        <div className="flex-1" />

        {/* Legend toggle */}
        <button type="button" className={clsx("btn p-1.5", showLegend && "text-gx-accent")} onClick={() => setShowLegend(!showLegend)} title="Legend / help">
          <HelpCircle size={14} />
        </button>

        {/* Remove edge button (visible when edge selected) */}
        {selectedEdge && (
          <button type="button" className="btn flex items-center gap-1 text-xs text-gx-error/70 hover:text-gx-error" onClick={() => handleDeleteEdge(selectedEdge)} title="Remove connection (Delete)">
            <Trash2 size={14} /> Remove Edge
          </button>
        )}

        {/* Run controls */}
        {state.phase === "running" ? (
          <button type="button" className="btn btn-danger flex items-center gap-1 text-xs" onClick={actions.cancel}>
            <Square size={14} /> Cancel
          </button>
        ) : (
          <button type="button" className="btn btn-primary flex items-center gap-1 text-xs" onClick={() => actions.run()} disabled={state.phase !== "editing" && state.phase !== "done"}>
            <Play size={14} /> Run All
          </button>
        )}
        {state.phase === "done" && (
          <button type="button" className="btn text-xs" onClick={actions.resetRun}>Reset</button>
        )}

        {/* Delete workflow */}
        {state.activeFile && (
          <button type="button" className="btn text-xs text-gx-error/70 hover:text-gx-error" onClick={async () => { const ok = await mainProcess.dialogConfirm(`Delete "${state.activeFile}"?`); if (ok) actions.deleteWorkflow(state.activeFile!); }} title="Delete workflow">
            <Trash2 size={14} />
          </button>
        )}
      </div>

      {/* Editor tab */}
      {activeTab === "editor" && (
        <div className="flex min-h-0 flex-1 flex-col">
          <div className="flex min-h-0 flex-1">
            {/* Canvas */}
            <div className="min-w-0 flex-1">
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
                fitView
                deleteKeyCode={null}
                proOptions={{ hideAttribution: true }}
                className="bg-gx-bg"
              >
                <Background color="#363a46" gap={20} size={1} />
                <Controls className="!border-gx-border !bg-gx-surface [&_button]:!border-gx-border [&_button]:!bg-gx-surface [&_button]:!fill-gx-muted hover:[&_button]:!bg-gx-surface-hover" />
                <MiniMap className="!border-gx-border !bg-gx-surface" nodeColor="#4f8cff" maskColor="rgba(15,17,21,0.7)" />

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
              <StagePropertiesPanel
                stage={selectedStageData}
                runStatus={state.runState?.stages.find((s) => s.name === selectedStage) ?? null}
                onUpdate={(stage) => { withHistory("Update stage", () => actions.updateStage(stage)); }}
                onRename={(oldName, newName) => { withHistory("Rename", () => actions.renameStage(oldName, newName)); setSelectedStage(newName); }}
                onDelete={() => handleDeleteStage(selectedStage!)}
                onClose={() => setSelectedStage(null)}
                onRun={() => actions.run([selectedStage!])}
                isRunning={state.phase === "running"}
              />
            )}
          </div>

          {/* Output pane */}
          <WorkflowOutputPane />
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

function WorkflowOutputPane() {
  const [lines, setLines] = useState<Array<{ id: string; text: string; stream: string }>>([]);
  const [collapsed, setCollapsed] = useState(false);
  const scrollRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    const unsub = onAgentEvent((event) => {
      if (event.method === "progress/line") {
        const data = event.params as { op?: string; job_id?: string; line?: string; stream?: string };
        if (data.job_id?.startsWith("workflow_run") || data.op === "workflow_run") {
          setLines((prev) => {
            const next = [...prev, { id: `${Date.now()}-${prev.length}`, text: data.line ?? "", stream: data.stream ?? "stdout" }];
            return next.length > 500 ? next.slice(-500) : next;
          });
        }
      }
    });
    return unsub;
  }, []);

  useEffect(() => {
    if (scrollRef.current) {
      scrollRef.current.scrollTop = scrollRef.current.scrollHeight;
    }
  }, [lines]);

  return (
    <div className={clsx("shrink-0 border-t border-gx-border bg-gx-surface", collapsed ? "h-7" : "h-[160px]")}>
      <div className="flex h-7 items-center justify-between border-b border-gx-border px-3">
        <button
          type="button"
          className="flex items-center gap-1.5 text-[11px] font-medium text-gx-muted hover:text-gx-text"
          onClick={() => setCollapsed(!collapsed)}
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
          <button type="button" className="text-[10px] text-gx-muted hover:text-gx-text" onClick={() => setLines([])}>
            Clear
          </button>
        )}
      </div>
      {!collapsed && (
        <div ref={scrollRef} className="h-[calc(100%-28px)] overflow-auto px-3 py-1 font-mono text-[11px]">
          {lines.length === 0 ? (
            <div className="py-4 text-center text-gx-muted/60">Run a workflow to see output here.</div>
          ) : (
            lines.map((line) => (
              <div
                key={line.id}
                className={clsx("whitespace-pre-wrap break-all leading-relaxed", line.stream === "stderr" ? "text-gx-error/80" : "text-gx-text/80")}
              >
                {line.text}
              </div>
            ))
          )}
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
        <span>{total} total</span>
      </div>
      {runState.status === "running" && (
        <div className="mt-1.5 h-1 w-full overflow-hidden rounded-full bg-gx-bg">
          <div
            className="h-full rounded-full bg-gx-accent transition-all"
            style={{ width: `${((done + failed) / total) * 100}%` }}
          />
        </div>
      )}
    </div>
  );
}

// ─── Export ──────────────────────────────────────────────────────────────────

export function WorkflowEditorView({ repo }: Props) {
  return (
    <ReactFlowProvider>
      <WorkflowEditorInner repo={repo} />
    </ReactFlowProvider>
  );
}
