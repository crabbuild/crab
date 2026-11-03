/**
 * Runs `layoutGraph` asynchronously using `setTimeout` to yield the
 * main thread before the computation starts. This keeps the commit
 * list visible immediately while the graph computes in the background.
 *
 * We intentionally avoid Web Workers here: Electron's renderer loads
 * via `file://` and Vite's worker bundling requires a dev server or
 * specific build config that isn't guaranteed in all environments.
 * A single `setTimeout(0)` is enough to unblock the first paint —
 * the layout itself takes ~50–300 ms for large repos, which is
 * acceptable since the list is already visible.
 *
 * For very large commit sets (>2000), `layoutGraph` internally caps
 * the number of commits it processes to avoid O(n²) memory growth.
 *
 * Returns `{ graphData, computing }`:
 *   - `graphData` is null until the first layout completes.
 *   - `computing` is true while a layout is in flight.
 */

import { useEffect, useRef, useState } from "react";
import type { CommitRow } from "../ipc";
import type { GraphData } from "../lib/commit-graph";
import { layoutGraph } from "../lib/commit-graph";

let nextJobId = 0;

export function useCommitGraphAsync(commits: CommitRow[]): {
  graphData: GraphData | null;
  computing: boolean;
} {
  const [graphData, setGraphData] = useState<GraphData | null>(null);
  const [computing, setComputing] = useState(false);
  const latestJobId = useRef(-1);

  useEffect(() => {
    if (commits.length === 0) {
      setGraphData({ branches: [], vertices: [], maxX: 0 });
      setComputing(false);
      return;
    }

    const id = ++nextJobId;
    latestJobId.current = id;
    setComputing(true);

    // Yield to the browser so the commit list renders before we block
    // the main thread with the layout computation.
    const timer = setTimeout(() => {
      // Bail if a newer job has been queued (e.g. filter changed).
      if (id !== latestJobId.current) return;

      const t0 = performance.now();
      const data = layoutGraph(commits);
      const elapsed = performance.now() - t0;

      // Diagnostic: log graph layout stats so we can correlate with OOM.
      const totalLines = data.branches.reduce((sum, b) => sum + b.lines.length, 0);
      console.debug(
        `[timeline] graph layout: ${commits.length} commits → ` +
        `${data.branches.length} branches, ${totalLines} lines, ` +
        `maxX=${data.maxX}, took ${elapsed.toFixed(1)}ms`
      );
      if (data.maxX > 50) {
        console.warn(
          `[timeline] graph has ${data.maxX} lanes — likely a --all repo ` +
          `with many branches. Layout was capped to prevent OOM.`
        );
      }

      if (id !== latestJobId.current) return; // check again after sync work
      setGraphData(data);
      setComputing(false);
    }, 0);

    return () => {
      clearTimeout(timer);
    };
  }, [commits]);

  return { graphData, computing };
}
