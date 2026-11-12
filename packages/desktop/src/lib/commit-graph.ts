// Git commit graph layout — ported from vscode-git-graph (MIT).
//
// This is a faithful adaptation of mhutchie/vscode-git-graph's
// `determinePath` algorithm. The key insight: we build Branch
// objects by tracing first-parent chains. Each Branch owns a list
// of line segments (point-to-point). Lines ALWAYS connect two
// vertex positions — there are never "floating" segments.
//
// Adapted for our use case:
//   - Input is CommitRow[] (newest-first, from `git log`).
//   - Output is a set of branches (with line segments) and vertex
//     positions, which the renderer converts to SVG paths + circles.

import type { CommitRow } from "../ipc";

// ---- Types ----

export interface GraphPoint {
  x: number; // column (lane)
  y: number; // row index
}

export interface GraphLine {
  p1: GraphPoint;
  p2: GraphPoint;
  /** When true, the angular transition is locked to p1 (the line
   *  bends near p2). When false, it bends near p1. */
  lockedFirst: boolean;
}

export interface GraphBranch {
  color: number;
  lines: GraphLine[];
}

export interface GraphVertex {
  id: number;
  x: number;
  color: number;
  isCurrent: boolean;
}

export interface GraphData {
  branches: GraphBranch[];
  vertices: GraphVertex[];
  maxX: number;
}

// ---- Internal classes ----

const NULL_ID = -1;

class Vertex {
  readonly id: number;
  x = 0;
  private children: Vertex[] = [];
  private parents: Vertex[] = [];
  private nextParent = 0;
  private onBranch: Branch | null = null;
  private nextX = 0;
  private connections: Array<{ connectsTo: Vertex | null; onBranch: Branch }> = [];
  isCurrent = false;

  constructor(id: number) {
    this.id = id;
  }

  addChild(v: Vertex) { this.children.push(v); }
  addParent(v: Vertex) { this.parents.push(v); }
  hasParents() { return this.parents.length > 0; }
  getNextParent(): Vertex | null {
    return this.nextParent < this.parents.length ? this.parents[this.nextParent] : null;
  }
  registerParentProcessed() { this.nextParent++; }
  isMerge() { return this.parents.length > 1; }

  addToBranch(branch: Branch, x: number) {
    if (this.onBranch === null) {
      this.onBranch = branch;
      this.x = x;
    }
  }
  isNotOnBranch() { return this.onBranch === null; }
  isOnThisBranch(b: Branch) { return this.onBranch === b; }
  getBranch() { return this.onBranch; }

  getPoint(): GraphPoint { return { x: this.x, y: this.id }; }
  getNextPoint(): GraphPoint { return { x: this.nextX, y: this.id }; }

  getPointConnectingTo(vertex: Vertex | null, onBranch: Branch): GraphPoint | null {
    for (let i = 0; i < this.connections.length; i++) {
      if (this.connections[i].connectsTo === vertex && this.connections[i].onBranch === onBranch) {
        return { x: i, y: this.id };
      }
    }
    return null;
  }

  registerUnavailablePoint(x: number, connectsTo: Vertex | null, onBranch: Branch) {
    if (x === this.nextX) {
      this.nextX = x + 1;
      this.connections[x] = { connectsTo, onBranch };
    }
  }

  getColour(): number {
    return this.onBranch !== null ? this.onBranch.color : 0;
  }
}

class Branch {
  readonly color: number;
  lines: GraphLine[] = [];
  end = 0;

  constructor(color: number) {
    this.color = color;
  }

  addLine(p1: GraphPoint, p2: GraphPoint, lockedFirst: boolean) {
    this.lines.push({ p1, p2, lockedFirst });
  }
}

// ---- Layout ----

/**
 * Maximum number of commits the graph layout will process. Beyond
 * this the algorithm's O(n × active_lanes) memory and CPU cost
 * becomes problematic in Electron's renderer process. The UI still
 * shows all commits in the list — only the decorative graph gutter
 * is truncated.
 */
const GRAPH_LAYOUT_LIMIT = 2000;

/**
 * Maximum active lanes (branches) before the layout bails out.
 * With `--all` on repos with 1000+ branches, even 200 commits can
 * create hundreds of concurrent lanes. Each lane stores a connections
 * entry per vertex it passes through, causing O(lanes × commits)
 * memory growth. At 50 lanes the graph is already too dense to read;
 * beyond this cap we return whatever was computed so far.
 */
const MAX_LANES = 50;

export function layoutGraph(commits: CommitRow[]): GraphData {
  if (commits.length === 0) return { branches: [], vertices: [], maxX: 0 };

  // For very large histories, only compute the graph for the first N
  // commits. The remaining commits still appear in the list but
  // without graph decoration. This prevents the quadratic memory
  // growth in the connections array from causing OOM.
  const layoutCommits = commits.length > GRAPH_LAYOUT_LIMIT
    ? commits.slice(0, GRAPH_LAYOUT_LIMIT)
    : commits;

  const nullVertex = new Vertex(NULL_ID);
  const vertices: Vertex[] = [];
  const commitLookup: Record<string, number> = {};

  // Build vertices + parent/child links.
  for (let i = 0; i < layoutCommits.length; i++) {
    vertices.push(new Vertex(i));
    commitLookup[layoutCommits[i].sha] = i;
  }
  for (let i = 0; i < layoutCommits.length; i++) {
    for (let j = 0; j < layoutCommits[i].parents.length; j++) {
      const parentIdx = commitLookup[layoutCommits[i].parents[j]];
      if (parentIdx !== undefined) {
        vertices[i].addParent(vertices[parentIdx]);
        vertices[parentIdx].addChild(vertices[i]);
      } else {
        // Parent is off-screen.
        vertices[i].addParent(nullVertex);
      }
    }
  }

  // Mark HEAD.
  // The first commit with a "head" or "head_target" ref is current.
  for (let i = 0; i < layoutCommits.length; i++) {
    const refs = layoutCommits[i].refs ?? [];
    if (refs.some(r => r.kind === "head" || r.kind === "head_target")) {
      vertices[i].isCurrent = true;
      break;
    }
  }

  // Determine paths (branches).
  const branches: Branch[] = [];
  const availableColours: number[] = [];

  function getAvailableColour(startAt: number): number {
    for (let i = 0; i < availableColours.length; i++) {
      if (startAt > availableColours[i]) return i;
    }
    availableColours.push(0);
    return availableColours.length - 1;
  }

  function determinePath(startAt: number) {
    let i = startAt;
    let vertex = vertices[i];
    let parentVertex = vertex.getNextParent();
    let lastPoint = vertex.isNotOnBranch() ? vertex.getNextPoint() : vertex.getPoint();

    if (
      parentVertex !== null &&
      parentVertex.id !== NULL_ID &&
      vertex.isMerge() &&
      !vertex.isNotOnBranch() &&
      !parentVertex.isNotOnBranch()
    ) {
      // Merge between two vertices already on branches — draw a
      // connecting line from the merge vertex to the parent's branch.
      const parentBranch = parentVertex.getBranch()!;
      let foundPointToParent = false;
      for (i = startAt + 1; i < vertices.length; i++) {
        const curVertex = vertices[i];
        let curPoint = curVertex.getPointConnectingTo(parentVertex, parentBranch);
        if (curPoint !== null) {
          foundPointToParent = true;
        } else {
          curPoint = curVertex.getNextPoint();
        }
        parentBranch.addLine(
          lastPoint,
          curPoint,
          !foundPointToParent && curVertex !== parentVertex
            ? lastPoint.x < curPoint.x
            : true,
        );
        curVertex.registerUnavailablePoint(curPoint.x, parentVertex, parentBranch);
        lastPoint = curPoint;
        if (foundPointToParent) {
          vertex.registerParentProcessed();
          break;
        }
      }
    } else {
      // Normal branch — trace from vertex down through first-parents.
      const branch = new Branch(getAvailableColour(startAt));
      vertex.addToBranch(branch, lastPoint.x);
      vertex.registerUnavailablePoint(lastPoint.x, vertex, branch);

      for (i = startAt + 1; i < vertices.length; i++) {
        const curVertex = vertices[i];
        const curPoint =
          parentVertex === curVertex && !parentVertex!.isNotOnBranch()
            ? curVertex.getPoint()
            : curVertex.getNextPoint();

        branch.addLine(lastPoint, curPoint, lastPoint.x < curPoint.x);
        curVertex.registerUnavailablePoint(curPoint.x, parentVertex, branch);
        lastPoint = curPoint;

        if (parentVertex === curVertex) {
          vertex.registerParentProcessed();
          const parentVertexOnBranch = !parentVertex!.isNotOnBranch();
          parentVertex!.addToBranch(branch, curPoint.x);
          vertex = parentVertex!;
          parentVertex = vertex.getNextParent();
          if (parentVertex === null || parentVertexOnBranch) break;
        }
      }

      if (i === vertices.length && parentVertex !== null && parentVertex.id === NULL_ID) {
        vertex.registerParentProcessed();
      }

      branch.end = i;
      branches.push(branch);
      availableColours[branch.color] = i;
    }
  }

  // Main loop: process all vertices.
  let i = 0;
  while (i < vertices.length) {
    // Bail if lane count has blown past the safety cap. This prevents
    // O(lanes × commits) memory growth on repos with 1000+ branches.
    if (availableColours.length > MAX_LANES) break;

    if (vertices[i].getNextParent() !== null || vertices[i].isNotOnBranch()) {
      determinePath(i);
    } else {
      i++;
    }
  }

  // Compute max x for width calculation.
  let maxX = 0;
  for (const v of vertices) {
    const p = v.getNextPoint();
    if (p.x > maxX) maxX = p.x;
  }

  // Build output.
  const outBranches: GraphBranch[] = branches.map(b => ({
    color: b.color,
    lines: b.lines,
  }));
  const outVertices: GraphVertex[] = vertices.map(v => ({
    id: v.id,
    x: v.x,
    color: v.getColour(),
    isCurrent: v.isCurrent,
  }));

  return { branches: outBranches, vertices: outVertices, maxX };
}

// ---- Colour palette ----

export const PALETTE = Array.from({ length: 10 }, (_, index) => `rgb(var(--chart-${index + 1}))`);

export function colorForIndex(idx: number): string {
  return PALETTE[idx % PALETTE.length];
}

// ---- Trunk computation (for popover "contained in HEAD") ----

export function computeTrunk(commits: CommitRow[]): Set<string> {
  const byId = new Map<string, CommitRow>();
  for (const c of commits) byId.set(c.sha, c);
  const trunk = new Set<string>();
  let cursor: CommitRow | undefined = commits[0];
  while (cursor) {
    if (trunk.has(cursor.sha)) break;
    trunk.add(cursor.sha);
    cursor = cursor.parents[0] ? byId.get(cursor.parents[0]) : undefined;
  }
  return trunk;
}
