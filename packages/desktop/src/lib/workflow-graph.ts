import type { NodePosition, WorkflowDefinition, WorkflowStage } from "./workflow-types";

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
export const normalizeWorkflowPath = (path: string) => path.replace(/\\/g, "/").split("/").filter(part => part && part !== ".").join("/");

export function workflowLinks(workflows: WorkflowDefinition[]): WorkflowLink[] {
  const outputs = workflows.flatMap(workflow => workflow.stages.flatMap(stage => stage.outs.map(out => ({
    file: workflow.path, stage: stage.name, kind: out.kind,
    path: normalizeWorkflowPath(directory(workflow.path) + out.path),
  }))));
  return workflows.flatMap(workflow => workflow.stages.flatMap(stage => stage.deps.flatMap(dep => {
    if (dep.kind !== "path") return [];
    const path = normalizeWorkflowPath(directory(workflow.path) + dep.value);
    const producer = outputs.find(out => out.kind !== "directory" && out.path === path)
      ?? outputs.filter(out => out.kind === "directory" && (out.path === path || path.startsWith(out.path + "/")))
        .sort((a, b) => b.path.length - a.path.length)[0];
    if (!producer) return [];
    return [{ fromFile: producer.file, fromStage: producer.stage, toFile: workflow.path, toStage: stage.name, path }];
  })));
}

export function availableWorkflowOutputs(workflows: WorkflowDefinition[], activeFile: string) {
  const prefix = directory(activeFile);
  return workflows.filter(workflow => workflow.path !== activeFile).flatMap(workflow => workflow.stages.flatMap(stage =>
    stage.outs.flatMap(out => {
      const path = normalizeWorkflowPath(directory(workflow.path) + out.path);
      if (!path.startsWith(prefix)) return [];
      return [{ file: workflow.path, stage: stage.name, path: path.slice(prefix.length) }];
    }),
  ));
}

export function stageEdges(stages: WorkflowStage[]): Array<{ source: string; target: string }> {
  const names = new Set(stages.map(stage => stage.name));
  const inferred = workflowLinks([{ path: "crab.yaml", stages, params: [], metrics: [], plots: [], defaults: {} }])
    .map(link => ({ source: link.fromStage, target: link.toStage }));
  const explicit = stages.flatMap(stage => stage.deps.flatMap(dep => {
    const source = dep.kind === "stage_out" ? dep.value.split(":")[0] : "";
    return names.has(source) ? [{ source, target: stage.name }] : [];
  }));
  const unique = new Map([...inferred, ...explicit].map(edge => [JSON.stringify(edge), edge]));
  return [...unique.values()];
}

export function layoutStages(stages: WorkflowStage[]): Record<string, NodePosition> {
  const edges = stageEdges(stages);
  const remaining = new Set(stages.map(stage => stage.name));
  const layers: string[][] = [];
  // Cycles remain visible in a final row; the editor separately marks them.
  while (remaining.size) {
    const ready = [...remaining].filter(name => !edges.some(edge => edge.target === name && remaining.has(edge.source))).sort();
    if (!ready.length) { layers.push([...remaining].sort()); break; }
    layers.push(ready);
    ready.forEach(name => remaining.delete(name));
  }
  const positions: Record<string, NodePosition> = {};
  for (const [depth, layer] of layers.entries()) {
    // Keep siblings near their producers, reducing crossings at branch/merge rows.
    const parentCenter = (name: string) => {
      const parents = edges.filter(edge => edge.target === name).flatMap(edge => positions[edge.source] ? [positions[edge.source].x] : []);
      return parents.length ? parents.reduce((sum, x) => sum + x, 0) / parents.length : 0;
    };
    layer.sort((a, b) => parentCenter(a) - parentCenter(b) || a.localeCompare(b));
    layer.forEach((name, index) => {
      positions[name] = { x: (index - (layer.length - 1) / 2) * (STAGE_WIDTH + 64), y: depth * (STAGE_HEIGHT + 88) };
    });
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
