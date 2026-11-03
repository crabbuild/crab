/**
 * Web Worker for off-thread commit graph layout.
 *
 * The main thread posts a `CommitRow[]` array; the worker runs
 * `layoutGraph` and posts back the `GraphData`. This keeps the UI
 * thread free during the O(n) layout pass, which takes ~100–400 ms
 * for repos with thousands of commits.
 */

import { layoutGraph } from "./commit-graph";
import type { CommitRow } from "../ipc";
import type { GraphData } from "./commit-graph";

export interface WorkerRequest {
  id: number;
  commits: CommitRow[];
}

export interface WorkerResponse {
  id: number;
  data: GraphData;
}

self.onmessage = (e: MessageEvent<WorkerRequest>) => {
  const { id, commits } = e.data;
  const data = layoutGraph(commits);
  const response: WorkerResponse = { id, data };
  (self as unknown as Worker).postMessage(response);
};
