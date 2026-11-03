/**
 * Task Decomposition Engine for the Crab Desktop Orchestrator.
 *
 * Decomposes a high-level user prompt into a directed acyclic graph (DAG)
 * of subtasks. Each node in the graph represents an independent workspace
 * that can be assigned to an agent. The coordinator validates dependency
 * ordering, persists graphs, and manages workspace creation on approval.
 */

import { EventEmitter } from "node:events";
import { execFile as execFileCb } from "node:child_process";
import { promisify } from "node:util";
import * as crypto from "node:crypto";
import * as fs from "node:fs";
import * as path from "node:path";
import * as os from "node:os";

const execFile = promisify(execFileCb);

// ─── Types ───────────────────────────────────────────────────────────────────

export interface TaskNode {
  id: string;
  title: string;
  description: string;
  suggestedAgent: "claude-code" | "codex" | "custom";
  estimatedFiles: string[];
  dependencies: string[];
  status: "queued" | "running" | "reviewing" | "applied" | "failed";
  workspaceId?: string;
}

export interface TaskGraph {
  id: string;
  rootPrompt: string;
  nodes: TaskNode[];
  createdAt: number;
  status: "draft" | "approved" | "in-progress" | "completed";
}

interface PersistedGraphData {
  graphs: TaskGraph[];
}

export interface DecomposeResult {
  graph: TaskGraph;
  warnings: string[];
}

// ─── Constants ───────────────────────────────────────────────────────────────

const DEFAULT_PERSIST_PATH = path.join(
  os.homedir(),
  ".crab",
  "desktop",
  "agent-graphs.json",
);

// ─── Coordinator ─────────────────────────────────────────────────────────────

export class Coordinator extends EventEmitter {
  private graphs = new Map<string, TaskGraph>();
  private readonly persistPath: string;

  constructor(persistPath?: string) {
    super();
    this.persistPath = persistPath ?? DEFAULT_PERSIST_PATH;
    this.loadGraphs();
  }

  // ─── Public API ──────────────────────────────────────────────────────────

  /**
   * Decomposes a user prompt into a task graph by analysing the repository
   * structure and generating subtask breakdowns.
   *
   * Returns a draft TaskGraph for user review before approval.
   */
  async decompose(prompt: string, repoPath: string): Promise<DecomposeResult> {
    const warnings: string[] = [];

    // Analyse the repository structure for context.
    const fileTree = await this.getFileTree(repoPath);
    const moduleBoundaries = this.identifyModuleBoundaries(fileTree);

    // Build a structured decomposition prompt.

    // For now, generate a placeholder graph from the prompt analysis.
    // In production, this would call the built-in AI agent.
    const nodes = this.generateTaskNodes(prompt, moduleBoundaries);

    const graph: TaskGraph = {
      id: crypto.randomUUID(),
      rootPrompt: prompt,
      nodes,
      createdAt: Date.now(),
      status: "draft",
    };

    // Validate the graph structure.
    const validationErrors = this.validateGraph(graph);
    if (validationErrors.length > 0) {
      warnings.push(...validationErrors);
    }

    this.graphs.set(graph.id, graph);
    await this.persistGraphs();

    this.emit("orchestrator:graph-ready", { graph });
    return { graph, warnings };
  }

  /**
   * Approves a draft task graph, transitioning it to "approved" status.
   * Returns the list of task node IDs that are ready to start (root nodes).
   */
  async approveGraph(graphId: string): Promise<string[]> {
    const graph = this.graphs.get(graphId);
    if (!graph) {
      throw new Error(`Task graph not found: ${graphId}`);
    }
    if (graph.status !== "draft") {
      throw new Error(`Cannot approve graph in status "${graph.status}". Expected "draft".`);
    }

    graph.status = "approved";
    await this.persistGraphs();

    // Identify root nodes (no dependencies) that can start immediately.
    const rootNodeIds = graph.nodes
      .filter((node) => node.dependencies.length === 0)
      .map((node) => node.id);

    this.emit("orchestrator:graph-approved", { graphId, rootNodeIds });
    return rootNodeIds;
  }

  /**
   * Updates the status of a task node within a graph. When a node reaches
   * "reviewing" or "applied", checks if dependent nodes can now start.
   */
  async updateNodeStatus(
    graphId: string,
    nodeId: string,
    status: TaskNode["status"],
    workspaceId?: string,
  ): Promise<string[]> {
    const graph = this.graphs.get(graphId);
    if (!graph) {
      throw new Error(`Task graph not found: ${graphId}`);
    }

    const node = graph.nodes.find((n) => n.id === nodeId);
    if (!node) {
      throw new Error(`Task node not found: ${nodeId}`);
    }

    node.status = status;
    if (workspaceId) {
      node.workspaceId = workspaceId;
    }

    // Check if all nodes are complete.
    const allComplete = graph.nodes.every(
      (n) => n.status === "applied" || n.status === "failed",
    );
    if (allComplete) {
      graph.status = "completed";
    } else if (graph.status === "approved") {
      graph.status = "in-progress";
    }

    await this.persistGraphs();

    // Find newly unblocked nodes.
    const unblockedIds = this.getUnblockedNodes(graph);
    if (unblockedIds.length > 0) {
      this.emit("orchestrator:nodes-unblocked", { graphId, nodeIds: unblockedIds });
    }

    return unblockedIds;
  }

  /**
   * Links a workspace ID to a task node.
   */
  async linkWorkspace(graphId: string, nodeId: string, workspaceId: string): Promise<void> {
    const graph = this.graphs.get(graphId);
    if (!graph) {
      throw new Error(`Task graph not found: ${graphId}`);
    }

    const node = graph.nodes.find((n) => n.id === nodeId);
    if (!node) {
      throw new Error(`Task node not found: ${nodeId}`);
    }

    node.workspaceId = workspaceId;
    await this.persistGraphs();
  }

  /**
   * Adaptive re-planning: filters the graph to queued nodes and
   * re-decomposes them based on what has already been completed.
   * Called after a subtask completes to adjust the remaining plan.
   */
  async adaptiveReplan(graphId: string): Promise<DecomposeResult | null> {
    const graph = this.graphs.get(graphId);
    if (!graph) {
      throw new Error(`Task graph not found: ${graphId}`);
    }

    // Only re-plan graphs that are in progress.
    if (graph.status !== "in-progress") return null;

    // Filter to queued nodes only.
    const queuedNodes = graph.nodes.filter((n) => n.status === "queued");
    if (queuedNodes.length === 0) return null;

    // Build context from completed nodes.
    const completedTitles = graph.nodes
      .filter((n) => n.status === "applied")
      .map((n) => n.title);

    const remainingPrompt = [
      `Original goal: ${graph.rootPrompt}`,
      "",
      `Already completed: ${completedTitles.join(", ") || "none"}`,
      "",
      `Remaining tasks to re-evaluate:`,
      ...queuedNodes.map((n) => `- ${n.title}: ${n.description}`),
    ].join("\n");

    // Re-decompose the remaining work.
    const newNodes = this.generateTaskNodes(remainingPrompt, []);

    // Replace queued nodes with re-decomposed nodes, preserving
    // dependency links to completed nodes.
    const completedIds = graph.nodes
      .filter((n) => n.status === "applied" || n.status === "running" || n.status === "reviewing")
      .map((n) => n.id);

    for (const node of newNodes) {
      // New nodes depend on all currently running/completed nodes
      // that the original queued nodes depended on.
      const originalDeps = queuedNodes.flatMap((q) => q.dependencies);
      node.dependencies = [...new Set(originalDeps.filter((d) => completedIds.includes(d)))];
    }

    // Remove old queued nodes and add new ones.
    graph.nodes = graph.nodes.filter((n) => n.status !== "queued");
    graph.nodes.push(...newNodes);

    // Validate the updated graph.
    const warnings = this.validateGraph(graph);
    await this.persistGraphs();

    this.emit("orchestrator:graph-replanned", { graphId, newNodes });
    return { graph, warnings };
  }

  /** Returns a task graph by ID. */
  getGraph(graphId: string): TaskGraph | undefined {
    return this.graphs.get(graphId);
  }

  /** Returns all task graphs. */
  getAllGraphs(): TaskGraph[] {
    return Array.from(this.graphs.values());
  }

  /** Removes a task graph. */
  async removeGraph(graphId: string): Promise<void> {
    this.graphs.delete(graphId);
    await this.persistGraphs();
  }

  // ─── Dependency Validation ───────────────────────────────────────────────

  /**
   * Validates a task graph for structural correctness:
   * - No circular dependencies
   * - All referenced dependency IDs exist
   * - At least one root node (no dependencies)
   */
  validateGraph(graph: TaskGraph): string[] {
    const errors: string[] = [];
    const nodeIds = new Set(graph.nodes.map((n) => n.id));

    // Check that all dependency references are valid.
    for (const node of graph.nodes) {
      for (const depId of node.dependencies) {
        if (!nodeIds.has(depId)) {
          errors.push(
            `Node "${node.title}" references unknown dependency: ${depId}`,
          );
        }
        if (depId === node.id) {
          errors.push(`Node "${node.title}" depends on itself`);
        }
      }
    }

    // Check for at least one root node.
    const hasRoot = graph.nodes.some((n) => n.dependencies.length === 0);
    if (!hasRoot) {
      errors.push("Graph has no root node (all nodes have dependencies)");
    }

    // Check for circular dependencies.
    if (this.hasCycle(graph)) {
      errors.push("Graph contains circular dependencies");
    }

    return errors;
  }

  // ─── Private Helpers ─────────────────────────────────────────────────────

  /**
   * Detects cycles in the task graph using DFS with colouring.
   */
  private hasCycle(graph: TaskGraph): boolean {
    const WHITE = 0;
    const GRAY = 1;
    const BLACK = 2;

    const colour = new Map<string, number>();
    for (const node of graph.nodes) {
      colour.set(node.id, WHITE);
    }

    // Build adjacency list (node → nodes that depend on it).
    const adj = new Map<string, string[]>();
    for (const node of graph.nodes) {
      if (!adj.has(node.id)) adj.set(node.id, []);
      for (const depId of node.dependencies) {
        if (!adj.has(depId)) adj.set(depId, []);
        adj.get(depId)!.push(node.id);
      }
    }

    const dfs = (nodeId: string): boolean => {
      colour.set(nodeId, GRAY);
      for (const neighbour of adj.get(nodeId) ?? []) {
        if (colour.get(neighbour) === GRAY) return true;
        if (colour.get(neighbour) === WHITE && dfs(neighbour)) return true;
      }
      colour.set(nodeId, BLACK);
      return false;
    };

    for (const node of graph.nodes) {
      if (colour.get(node.id) === WHITE) {
        if (dfs(node.id)) return true;
      }
    }

    return false;
  }

  /**
   * Returns IDs of nodes whose dependencies are all satisfied
   * (status "applied" or "reviewing") and that are still "queued".
   */
  private getUnblockedNodes(graph: TaskGraph): string[] {
    const completedIds = new Set(
      graph.nodes
        .filter((n) => n.status === "applied" || n.status === "reviewing")
        .map((n) => n.id),
    );

    return graph.nodes
      .filter(
        (node) =>
          node.status === "queued" &&
          node.dependencies.every((depId) => completedIds.has(depId)),
      )
      .map((n) => n.id);
  }

  /**
   * Gets the file tree of a repository (top 2 levels) for context.
   */
  private async getFileTree(repoPath: string): Promise<string[]> {
    try {
      const { stdout } = await execFile(
        "git",
        ["ls-tree", "-r", "--name-only", "HEAD"],
        { cwd: repoPath, maxBuffer: 1024 * 1024 },
      );
      return stdout.trim().split("\n").filter(Boolean);
    } catch {
      return [];
    }
  }

  /**
   * Identifies module boundaries from the file tree by grouping
   * files into top-level directories.
   */
  private identifyModuleBoundaries(fileTree: string[]): string[] {
    const topDirs = new Set<string>();
    for (const file of fileTree) {
      const parts = file.split("/");
      if (parts.length > 1) {
        topDirs.add(parts[0]);
      }
    }
    return Array.from(topDirs);
  }


  /**
   * Generates placeholder task nodes from the prompt.
   * In production, this would parse AI-generated structured output.
   */
  private generateTaskNodes(prompt: string, _modules: string[]): TaskNode[] {
    // Single-node fallback: the entire prompt as one task.
    return [
      {
        id: crypto.randomUUID(),
        title: prompt.slice(0, 80),
        description: prompt,
        suggestedAgent: "claude-code",
        estimatedFiles: [],
        dependencies: [],
        status: "queued",
      },
    ];
  }

  // ─── Persistence ─────────────────────────────────────────────────────────

  private async persistGraphs(): Promise<void> {
    const data: PersistedGraphData = {
      graphs: Array.from(this.graphs.values()),
    };

    const dir = path.dirname(this.persistPath);
    try {
      fs.mkdirSync(dir, { recursive: true });
      fs.writeFileSync(this.persistPath, JSON.stringify(data, null, 2), "utf-8");
    } catch (err) {
      this.emit("orchestrator:error", {
        message: `Failed to persist graphs: ${err instanceof Error ? err.message : String(err)}`,
      });
    }
  }

  private loadGraphs(): void {
    try {
      if (!fs.existsSync(this.persistPath)) return;

      const raw = fs.readFileSync(this.persistPath, "utf-8");
      const data: PersistedGraphData = JSON.parse(raw);

      if (!data.graphs || !Array.isArray(data.graphs)) return;

      for (const graph of data.graphs) {
        this.graphs.set(graph.id, graph);
      }
    } catch {
      // Corrupted or unreadable — start fresh.
    }
  }
}
