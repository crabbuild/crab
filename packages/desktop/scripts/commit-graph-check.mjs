// Standalone layout sanity check.
//
// Runs the `layoutGraph` algorithm against a handful of canonical
// DAG shapes and prints them as ASCII art so a human (or a future
// regression test) can eyeball whether the lane assignment makes
// sense. Not a unit test — just a visual debug aid to be honest
// about what the algorithm does.
//
// Run: `node scripts/commit-graph-check.mjs`

import { execSync } from "node:child_process";
import { mkdirSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

// Minimal ts-node-free transpile: esbuild only via `npx` if
// available. Simpler approach: import via a tiny TS->JS compile
// through `npx tsc` on the file into a tmp dir.
const tmp = join(tmpdir(), `crab-graph-${Date.now()}`);
mkdirSync(tmp, { recursive: true });
execSync(
  `npx --yes esbuild src/lib/commit-graph.ts --bundle --format=esm --platform=node --outfile=${tmp}/commit-graph.mjs`,
  { stdio: "inherit" },
);
const { layoutGraph } = await import(`file://${tmp}/commit-graph.mjs`);

/** Build a synthetic commits list newest-first. */
function mkCommit(sha, parents, refs = []) {
  return {
    sha,
    parents,
    author: "a",
    date: "2026-01-01T00:00:00Z",
    subject: sha,
    refs,
  };
}

// DAG 1: linear trunk.
//   M -> B -> A
const linear = [
  mkCommit("M", ["B"]),
  mkCommit("B", ["A"]),
  mkCommit("A", []),
];

// DAG 2: topic branch merged back.
//   X (merge) -> M, T2
//   T2 -> T1
//   M -> T1? no — M is trunk parent; T1 -> A
//   Actual parents:
//     X: parents [M, T2]   ← trunk + topic
//     M: parents [A]       ← older trunk
//     T2: parents [T1]     ← older topic
//     T1: parents [A]      ← topic root
//     A: parents []        ← root
const merge = [
  mkCommit("X", ["M", "T2"], [{ kind: "head", name: "HEAD" }]),
  mkCommit("M", ["A"]),
  mkCommit("T2", ["T1"]),
  mkCommit("T1", ["A"]),
  mkCommit("A", []),
];

// DAG 3: two parallel branches meeting at a shared root.
//   F1, F2 are branch tips of two unmerged features.
//     F1: parents [A]
//     F2: parents [A]
//     A: parents []
const fork = [
  mkCommit("F1", ["A"], [{ kind: "branch", name: "f1" }]),
  mkCommit("F2", ["A"], [{ kind: "branch", name: "f2" }]),
  mkCommit("A", []),
];

// DAG 4: octopus — one commit with 3 parents.
//   O: parents [M, P1, P2]
//   M, P1, P2 each have parents [A]
//   A: []
const octopus = [
  mkCommit("O", ["M", "P1", "P2"]),
  mkCommit("M", ["A"]),
  mkCommit("P1", ["A"]),
  mkCommit("P2", ["A"]),
  mkCommit("A", []),
];

function render(rows) {
  const maxLane = rows.reduce(
    (m, r) =>
      Math.max(
        m,
        r.commitLane,
        ...r.activeLanes.map((s) => s.lane),
        ...r.parentLanes.map((p) => p.lane),
      ),
    0,
  );
  const cols = maxLane + 1;
  const lines = [];
  for (const r of rows) {
    const cells = Array(cols).fill(" ");
    for (const s of r.activeLanes) cells[s.lane] = "|";
    cells[r.commitLane] = "*";
    lines.push(
      cells.join(" ") +
        `   ${r.commitSha.padEnd(3)} lane=${r.commitLane} color=${r.commitColor} parents=${r.parentLanes.map(p => p.lane).join(",") ?? ""}`,
    );
  }
  return lines.join("\n");
}

function check(name, commits) {
  console.log(`=== ${name} ===`);
  const rows = layoutGraph(commits);
  console.log(render(rows));
  console.log("");
}

check("linear trunk", linear);
check("topic branch merged back", merge);
check("two parallel tips", fork);
check("octopus merge (3 parents)", octopus);

writeFileSync(join(tmp, ".done"), "");
console.log(`(temp artifacts in ${tmp})`);
