#!/usr/bin/env node
import { spawn } from "node:child_process";
import { createInterface } from "node:readline";

// Forward the installed CLI, losing only the second turn/start response.
// Close the client connection after that real turn finishes, so the fixture
// proves that an unacknowledged input can already have changed the workspace.
const child = spawn("codex", process.argv.slice(2), { stdio: ["pipe", "pipe", "pipe"] });
let starts = 0;
let droppedId;
let droppedTurn;
let closed = false;
const completed = new Set();
const closeOutput = () => {
  if (closed) return;
  closed = true;
  process.stdout.end();
};
createInterface({ input: process.stdin }).on("line", line => {
  const frame = JSON.parse(line);
  if (frame.method === "turn/start" && ++starts === 2) droppedId = frame.id;
  child.stdin.write(line + "\n");
}).on("close", () => child.stdin.end());
createInterface({ input: child.stdout }).on("line", line => {
  const frame = JSON.parse(line);
  if (!frame.method && frame.id === droppedId && frame.result?.turn?.id) {
    droppedTurn = frame.result.turn.id;
    if (completed.has(droppedTurn)) closeOutput();
    return;
  }
  if (!closed) process.stdout.write(line + "\n");
  if (frame.method === "turn/completed") {
    const turnId = frame.params?.turn?.id;
    completed.add(turnId);
    if (turnId === droppedTurn) closeOutput();
  }
});
child.stderr.pipe(process.stderr);
process.on("SIGTERM", () => child.kill("SIGTERM"));
process.on("SIGINT", () => child.kill("SIGINT"));
child.once("error", error => { process.stderr.write(error.message); process.exit(1); });
child.once("close", code => { closeOutput(); process.exit(code ?? 1); });
