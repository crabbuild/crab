import { describe, expect, it } from "vitest";
import { availableWorkflowOutputs, layoutStages, stageEdges, STAGE_HEIGHT, STAGE_WIDTH, workflowLinks, workflowPathError } from "../src/lib/workflow-graph";
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
  });

  it.each(["crab.yaml", "prepare.workflow.yaml", "pipelines/eval.workflow.yaml", "data/crab.yaml"])("accepts discoverable workflow path %s", path => {
    expect(workflowPathError(path)).toBeNull();
  });
  it.each(["pipeline.yaml", "a.yml", "../crab.yaml", "/crab.yaml", ".hidden/crab.yaml", "bad name.workflow.yaml"])("rejects undiscoverable or unsafe path %s", path => {
    expect(workflowPathError(path)).not.toBeNull();
  });
});
