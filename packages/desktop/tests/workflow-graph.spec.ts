import { describe, expect, it } from "vitest";
import { availableWorkflowOutputs, cycleStages, layoutStages, renameStages, stageEdges, stageRunStatus, STAGE_HEIGHT, STAGE_WIDTH, workflowLinks, workflowPathError } from "../src/lib/workflow-graph";
import type { WorkflowDefinition, WorkflowStage } from "../src/lib/workflow-types";

const stage = (name: string, deps: WorkflowStage["deps"] = [], outs: WorkflowStage["outs"] = []): WorkflowStage => ({ name, cmd: { kind: "shell", value: "true" }, deps, outs, params: [], env: "inherit" });
const workflow = (path: string, stages: WorkflowStage[]): WorkflowDefinition => ({ path, stages, defaults: {}, params: [], metrics: [], plots: [] });

describe("workflow graph", () => {
  it("lays out a branch and merge without overlapping readable nodes", () => {
    const stages = [stage("start", [], [{ path: "raw", kind: "file" }]),
      stage("left", [{ kind: "path", value: "raw" }], [{ path: "left", kind: "file" }]),
      stage("right", [{ kind: "path", value: "raw" }], [{ path: "right", kind: "file" }]),
      stage("merge", [{ kind: "stage_out", value: "left:left" }, { kind: "path", value: "right" }])];
    const layout = layoutStages(stages);
    for (const { source, target } of stageEdges(stages)) expect(layout[target].y - layout[source].y).toBeGreaterThan(STAGE_HEIGHT);
    expect(Math.abs(layout.left.x - layout.right.x)).toBeGreaterThan(STAGE_WIDTH);
    expect(layout.merge.x).toBe(layout.start.x);
    expect(layoutStages([...stages].reverse())).toEqual(layout);
  });

  it("lays out valid names that are also object prototype keys", () => {
    const stages = [stage("constructor"), stage("toString"), stage("__proto__")];
    const layout = layoutStages(stages);
    expect(Object.keys(layout)).toHaveLength(3);
    for (const position of Object.values(layout)) expect(Number.isFinite(position.x) && Number.isFinite(position.y)).toBe(true);
  });

  it("places newly added stages clear of manually positioned stages", () => {
    const stages = [stage("a"), stage("b"), stage("c")];
    const fixed = { a: { x: 0, y: 0 }, b: { x: 304, y: 0 } };
    const layout = layoutStages(stages, fixed);
    expect(layout.a).toEqual(fixed.a);
    expect(layout.b).toEqual(fixed.b);
    for (const name of ["a", "b"]) expect(Math.abs(layout.c.x - layout[name].x)).toBeGreaterThanOrEqual(STAGE_WIDTH);
  });

  it("lays out a large, dense branching DAG deterministically", () => {
    const stages = Array.from({ length: 512 }, (_, index) => stage(`stage_${index}`, index < 16 ? [] :
      [0, 1, 3].map(offset => ({ kind: "stage_out" as const, value: `stage_${Math.floor(index / 16 - 1) * 16 + ((index + offset) % 16)}:out` }))));
    const layout = layoutStages(stages);
    expect(Object.keys(layout)).toHaveLength(stages.length);
    for (const edge of stageEdges(stages)) expect(layout[edge.target].y - layout[edge.source].y).toBeGreaterThan(STAGE_HEIGHT);
    expect(layoutStages([...stages].reverse())).toEqual(layout);
  });

  it("keeps cyclic stages visible without blocking layout", () => {
    const layout = layoutStages([stage("a", [{ kind: "stage_out", value: "b:out" }]), stage("b", [{ kind: "stage_out", value: "a:out" }])]);
    expect(Math.abs(layout.a.x - layout.b.x)).toBeGreaterThan(STAGE_WIDTH);
  });

  it("deduplicates explicit and inferred dependencies", () => {
    const stages = [stage("source", [], [{ path: "out", kind: "file" }]), stage("target", [{ kind: "path", value: "./out" }, { kind: "stage_out", value: "source:out" }])];
    expect(stageEdges(stages)).toEqual([{ source: "source", target: "target" }]);
  });

  it("matches cross-file outputs by directory ownership and longest directory prefix", () => {
    const workflows = [workflow("data/crab.yaml", [stage("all", [], [{ path: "data", kind: "directory" }]), stage("specific", [], [{ path: "data/sub", kind: "directory" }])]),
      workflow("train.workflow.yaml", [stage("fit", [{ kind: "path", value: "./data/data/sub/features.csv" }])])];
    expect(workflowLinks(workflows)).toEqual([{ fromFile: "data/crab.yaml", fromStage: "specific", toFile: "train.workflow.yaml", toStage: "fit", path: "data/data/sub/features.csv" }]);
  });

  it("does not invent edges from parent traversals or stage-like path strings", () => {
    const workflows = [workflow("prepare.workflow.yaml", [stage("source", [], [{ path: "out", kind: "file" }])]),
      workflow("sub/crab.yaml", [stage("target", [{ kind: "path", value: "../out" }, { kind: "path", value: "source:out" }])])];
    expect(workflowLinks(workflows)).toEqual([]);
  });

  it("only offers outputs addressable from the active workflow directory", () => {
    const workflows = [workflow("prepare.workflow.yaml", [stage("source", [], [{ path: "shared/out", kind: "file" }, { path: "root.out", kind: "file" }])]), workflow("shared/train.workflow.yaml", [stage("fit")])];
    expect(availableWorkflowOutputs(workflows, "shared/train.workflow.yaml")).toEqual([{ file: "prepare.workflow.yaml", stage: "source", path: "out" }]);
    expect(availableWorkflowOutputs(workflows, "shared/train.workflow.yaml", null)).toEqual([{ file: "prepare.workflow.yaml", stage: "source", path: "out" }]);
    expect(availableWorkflowOutputs(workflows, "shared/train.workflow.yaml", ".")).toEqual([
      { file: "prepare.workflow.yaml", stage: "source", path: "shared/out" },
      { file: "prepare.workflow.yaml", stage: "source", path: "root.out" },
    ]);
  });

  it("uses physical working directory paths for inferred and explicit edges", () => {
    const stages = [{ ...stage("produce", [], [{ path: "out", kind: "file" }]), wdir: "work" },
      stage("consume", [{ kind: "path", value: "work/out" }]),
      stage("explicit", [{ kind: "stage_out", value: "produce:work/out" }])];
    expect(stageEdges(stages)).toEqual([{ source: "produce", target: "consume" }, { source: "produce", target: "explicit" }]);
  });

  it("collapses expanded execution edges to their editable templates", () => {
    const stages = [stage("prepare"), stage("merge")];
    const executions = [
      { name: "prepare@a", source: "prepare", deps: [], outs: [{ path: "a.txt", kind: "file" as const }] },
      { name: "prepare@b", source: "prepare", deps: [], outs: [{ path: "b.txt", kind: "file" as const }] },
      { name: "merge", source: "merge", deps: [{ kind: "path" as const, value: "a.txt" }, { kind: "path" as const, value: "b.txt" }], outs: [] },
    ];
    expect(stageEdges(stages, executions)).toEqual([{ source: "prepare", target: "merge" }]);
    expect(layoutStages(stages, {}, executions).merge.y).toBeGreaterThan(STAGE_HEIGHT);
  });

  it("does not report cycles caused only by folding interleaved template executions", () => {
    const stages = [{ ...stage("first"), foreach: ["zero", "one"] }, { ...stage("second"), foreach: ["zero", "one"] }];
    const executions = [
      { name: "first@zero", source: "first", deps: [], outs: [{ path: "first0", kind: "file" as const }] },
      { name: "second@zero", source: "second", deps: [{ kind: "path" as const, value: "first0" }], outs: [{ path: "second0", kind: "file" as const }] },
      { name: "first@one", source: "first", deps: [{ kind: "path" as const, value: "second0" }], outs: [{ path: "first1", kind: "file" as const }] },
      { name: "second@one", source: "second", deps: [{ kind: "path" as const, value: "first1" }], outs: [] },
    ];
    expect(cycleStages(stages, executions).size).toBe(0);
    expect(cycleStages(stages, executions.map(stage => stage.name === "first@zero" ? { ...stage, deps: [{ kind: "path", value: "second0" }] } : stage))).toEqual(new Set(["first", "second"]));
  });

  it("renames a template's references while preserving explicitly authored expanded names", () => {
    const stages = [{ ...stage("train", [], [{ path: "${item}.txt", kind: "file" }]), foreach: ["x"] },
      stage("train@manual", [], [{ path: "manual.txt", kind: "file" }]),
      stage("consumer", [{ kind: "stage_out", value: "train@x:x.txt" }, { kind: "stage_out", value: "train@manual:manual.txt" }])];
    const renamed = renameStages(stages, "train", "fit");
    expect(renamed[0].name).toBe("fit");
    expect(renamed[2].deps.map(dep => dep.value)).toEqual(["fit@x:x.txt", "train@manual:manual.txt"]);
  });

  it("aggregates every template execution before reporting completion", () => {
    const executions = ["a", "b"].map(key => ({ name: `prepare@${key}`, source: "prepare", deps: [], outs: [] }));
    const run = { run_id: "test", status: "running" as const, output: [], stages: [
      { name: "prepare@a", status: "done" as const }, { name: "prepare@b", status: "pending" as const },
    ] };
    expect(stageRunStatus("prepare", executions, run)).toBe("pending");
    expect(stageRunStatus("prepare", executions, { ...run, stages: run.stages.map(stage => ({ ...stage, status: "cached" })) })).toBe("cached");
    expect(stageRunStatus("prepare", executions, { ...run, stages: run.stages.map(stage => ({ ...stage, status: "skipped" })) })).toBe("skipped");
  });

  it.each(["crab.yaml", "prepare.workflow.yaml", "pipelines/eval.workflow.yaml", "data/crab.yaml"])("accepts discoverable workflow path %s", path => {
    expect(workflowPathError(path)).toBeNull();
  });
  it.each(["pipeline.yaml", "a.yml", "../crab.yaml", "/crab.yaml", ".hidden/crab.yaml", "bad name.workflow.yaml"])("rejects undiscoverable or unsafe path %s", path => {
    expect(workflowPathError(path)).not.toBeNull();
  });
});
