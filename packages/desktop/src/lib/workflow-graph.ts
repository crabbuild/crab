import type { NodePosition, WorkflowDefinition, WorkflowExecution, WorkflowRunState, WorkflowStage, StageRunStatus } from "./workflow-types";

export const STAGE_WIDTH = 240;
export const STAGE_HEIGHT = 112;

export interface WorkflowLink {
  fromFile: string;
  fromStage: string;
  toFile: string;
  toStage: string;
  path: string;
}

const directory = (file: string) => file.slice(0, Math.max(0, file.lastIndexOf("/") + 1));
// Match the engine's path components: discard '.', preserve '..' rather than
// inventing cross-directory edges that the recursive DAG cannot resolve.
export const normalizeWorkflowPath = (path: string) => (path.startsWith("/") ? "/" : "") + path.replace(/\\/g, "/").split("/").filter(part => part && part !== ".").join("/");

function graphStages(workflow: WorkflowDefinition) {
  return workflow.executions?.map(stage => ({ ...stage, owner: stage.source }))
    ?? workflow.stages.map(stage => ({ ...stage, owner: stage.name }));
}

export function artifactPath(file: string, wdir: string | null | undefined, path: string) {
  if (path.startsWith("/") || /^[A-Za-z]:[\\/]/.test(path)) return normalizeWorkflowPath(path);
  return normalizeWorkflowPath((wdir ? wdir + "/" : directory(file)) + path);
}

export function workflowLinks(workflows: WorkflowDefinition[]): WorkflowLink[] {
  const outputs = workflows.flatMap(workflow => graphStages(workflow).flatMap(stage => stage.outs.map(out => ({
    file: workflow.path, stage: stage.owner, execution: stage.name, wdir: stage.wdir, kind: out.kind,
    path: artifactPath(workflow.path, stage.wdir, out.path),
  }))));
  return workflows.flatMap(workflow => graphStages(workflow).flatMap(stage => stage.deps.flatMap(dep => {
    if (dep.kind !== "path" && dep.kind !== "stage_out") return [];
    const [name, ...rest] = dep.value.split(":");
    const path = dep.kind === "path" ? artifactPath(workflow.path, stage.wdir, dep.value)
      : normalizeWorkflowPath(rest.join(":"));
    // Explicit StageOut paths include the producer's wdir, whereas local path
    // deps use the consumer's wdir. Discovery prefixes only non-wdir paths.
    const producer = dep.kind === "stage_out"
      ? outputs.find(out => out.file === workflow.path && out.execution === name &&
        out.path === normalizeWorkflowPath((out.wdir ? "" : directory(workflow.path)) + rest.join(":")))
      : outputs.find(out => out.kind !== "directory" && out.path === path)
        ?? outputs.filter(out => out.kind === "directory" && (out.path === path || path.startsWith(out.path + "/")))
          .sort((a, b) => b.path.length - a.path.length)[0];
    if (!producer) return [];
    return [{ fromFile: producer.file, fromStage: producer.stage, toFile: workflow.path, toStage: stage.owner, path: dep.kind === "path" ? path : producer.path }];
  })));
}

export function availableWorkflowOutputs(workflows: WorkflowDefinition[], activeFile: string, wdir?: string | null) {
  const base = wdir == null ? directory(activeFile) : normalizeWorkflowPath(wdir);
  const prefix = base && !base.endsWith("/") ? base + "/" : base;
  return workflows.filter(workflow => workflow.path !== activeFile).flatMap(workflow => graphStages(workflow).flatMap(stage =>
    stage.outs.flatMap(out => {
      const path = artifactPath(workflow.path, stage.wdir, out.path);
      if (!path.startsWith(prefix)) return [];
      return [{ file: workflow.path, stage: stage.owner, path: path.slice(prefix.length) }];
    }),
  ));
}

export function stageEdges(stages: WorkflowStage[], executions?: WorkflowExecution[]): Array<{ source: string; target: string }> {
  const names = new Set(stages.map(stage => stage.name));
  const inferred = workflowLinks([{ path: "crab.yaml", stages, executions, params: [], metrics: [], plots: [], defaults: {} }])
    .map(link => ({ source: link.fromStage, target: link.toStage }));
  const explicit = executions ? [] : stages.flatMap(stage => stage.deps.flatMap(dep => {
    const source = dep.kind === "stage_out" ? dep.value.split(":")[0] : "";
    return names.has(source) ? [{ source, target: stage.name }] : [];
  }));
  const templates = new Set(stages.filter(stage => stage.foreach || stage.matrix).map(stage => stage.name));
  const unique = new Map([...inferred, ...explicit].filter(edge => edge.source !== edge.target || !templates.has(edge.source)).map(edge => [JSON.stringify(edge), edge]));
  return [...unique.values()];
}

// Contracting execution nodes into templates can create apparent loops in an
// acyclic graph. Diagnose actual executions, then highlight their source owners.
export function cycleStages(stages: WorkflowStage[], executions?: WorkflowExecution[]): Set<string> {
  const stageNames = new Set((executions ?? stages).map(stage => stage.name));
  const graphEdges = executions ? stageEdges(stages, executions.map(stage => ({ ...stage, source: stage.name }))) : stageEdges(stages);

  // Build adjacency list: parent -> children (stages that depend on parent)
  const adjacency = new Map<string, string[]>();
  for (const name of stageNames) adjacency.set(name, []);

  for (const edge of graphEdges) adjacency.get(edge.source)!.push(edge.target);

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

  const owners = new Map(executions?.map(stage => [stage.name, stage.source]));
  return new Set([...cycleMembers].map(name => owners.get(name) ?? name));
}


export function renameStages(stages: WorkflowStage[], oldName: string, newName: string): WorkflowStage[] {
  const source = stages.find(stage => stage.name === oldName);
  const template = Boolean(source?.foreach || source?.matrix);
  const authored = new Set(stages.map(stage => stage.name));
  return stages.map(stage => ({
    ...stage, name: stage.name === oldName ? newName : stage.name,
    deps: stage.deps.map(dep => {
      if (dep.kind !== "stage_out") return dep;
      const producer = dep.value.split(":")[0];
      // An authored name wins over a template prefix, matching native ownership.
      const owned = producer === oldName || (template && !authored.has(producer) && producer.startsWith(oldName + "@"));
      return owned ? { ...dep, value: newName + dep.value.slice(oldName.length) } : dep;
    }),
  }));
}

export function stageRunStatus(source: string, executions: WorkflowExecution[] | undefined, run: WorkflowRunState | null): StageRunStatus {
  const names = new Set(executions?.filter(stage => stage.source === source).map(stage => stage.name) ?? [source]);
  const statuses = run?.stages.filter(stage => names.has(stage.name)).map(stage => stage.status) ?? [];
  if (!statuses.length) return "pending";
  if (statuses.includes("failed")) return "failed";
  if (statuses.includes("running")) return "running";
  if (statuses.includes("pending") || statuses.includes("queued")) return "pending";
  if (statuses.every(status => status === "skipped")) return "skipped";
  if (statuses.every(status => status === "cached")) return "cached";
  return "done";
}

export function layoutStages(stages: WorkflowStage[], fixed: Record<string, NodePosition> = {}, executions?: WorkflowExecution[]): Record<string, NodePosition> {
  const edges = stageEdges(stages, executions);
  const children = new Map(stages.map(stage => [stage.name, [] as string[]]));
  const parents = new Map(stages.map(stage => [stage.name, [] as string[]]));
  for (const edge of edges) {
    children.get(edge.source)!.push(edge.target);
    parents.get(edge.target)!.push(edge.source);
  }
  const degrees = new Map(stages.map(stage => [stage.name, parents.get(stage.name)!.length]));
  const remaining = new Set(stages.map(stage => stage.name));
  const layers: string[][] = [];
  let ready = [...remaining].filter(name => degrees.get(name) === 0).sort();
  while (ready.length) {
    layers.push(ready);
    const next: string[] = [];
    for (const name of ready) {
      remaining.delete(name);
      for (const child of children.get(name)!) {
        const count = degrees.get(child)! - 1;
        degrees.set(child, count);
        if (!count) next.push(child);
      }
    }
    ready = next.sort();
  }
  if (remaining.size) layers.push([...remaining].sort());
  const positions: Record<string, NodePosition> = Object.create(null);
  for (const stage of stages) {
    if (Object.hasOwn(fixed, stage.name)) positions[stage.name] = fixed[stage.name];
  }
  for (const [depth, layer] of layers.entries()) {
    const parentCenter = (name: string) => {
      const xs = parents.get(name)!.flatMap(parent => positions[parent] ? [positions[parent].x] : []);
      return xs.length ? xs.reduce((sum, x) => sum + x, 0) / xs.length : 0;
    };
    layer.sort((a, b) => parentCenter(a) - parentCenter(b) || a.localeCompare(b));
    for (const [index, name] of layer.entries()) {
      if (positions[name]) continue;
      const candidate = { x: (index - (layer.length - 1) / 2) * (STAGE_WIDTH + 64), y: depth * (STAGE_HEIGHT + 88) };
      // Preserve manual placements; only new nodes move around occupied space.
      let obstacle: NodePosition | undefined;
      while ((obstacle = Object.values(positions).find(position => Math.abs(candidate.x - position.x) < STAGE_WIDTH + 32 && Math.abs(candidate.y - position.y) < STAGE_HEIGHT + 32))) {
        candidate.x = obstacle.x + STAGE_WIDTH + 64;
      }
      positions[name] = candidate;
    }
  }
  return positions;
}

export function workflowPathError(path: string): string | null {
  const parts = path.split("/");
  const file = parts.pop() ?? "";
  if (parts.some(part => !/^[A-Za-z_][A-Za-z0-9_-]*$/.test(part)) ||
    (file !== "crab.yaml" && !/^[A-Za-z_][A-Za-z0-9_-]*\.workflow\.yaml$/.test(file))) {
    return "Use crab.yaml or name.workflow.yaml. Folder and workflow names must start with a letter or underscore and contain letters, digits, - or _.";
  }
  return null;
}
