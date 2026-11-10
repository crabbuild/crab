import { spawn, execFile } from "node:child_process";
import { randomUUID } from "node:crypto";
import { readFile } from "node:fs/promises";
import path from "node:path";
import { createInterface } from "node:readline";
import { promisify } from "node:util";

const execute = promisify(execFile);
const directory = path.resolve(process.argv[2] ?? `resources/native/linux-${process.arch}`);
const runtime = JSON.parse(await readFile(path.join(directory, "runtime.json"), "utf8"));
if (runtime.platform !== "linux" || !["arm64", "x64"].includes(runtime.arch)) throw new Error("Linux runtime identity is required");
const platform = `linux/${runtime.arch === "arm64" ? "arm64" : "amd64"}`;
const content = "older-linux-runtime\n";
const requests = [
  { method: "ping", params: {}, accepts: result => result.pong === runtime.version },
  { method: "open_local", params: { repo_id: "abi-proof", path: "/proof" }, accepts: result => result.repo_id === "abi-proof" },
  { method: "write_text", params: { repo_id: "abi-proof", path: "proof.txt", content }, accepts: result => result.written === true && result.size === content.length },
  { method: "read_text", params: { repo_id: "abi-proof", path: "proof.txt" }, accepts: result => result.text === content && result.truncated === false },
];

for (const [image, install] of [
  ["almalinux:8", "dnf -q install -y git libstdc++ ca-certificates"],
  ["debian:12-slim", "apt-get -qq update && apt-get -qq install -y git libstdc++6 ca-certificates"],
  ["amazonlinux:2023", "dnf -q install -y git libstdc++ ca-certificates"],
]) {
  const name = `crab-runtime-${randomUUID()}`;
  const script = `${install} </dev/null >&2
test "$(/runtime/crab --version)" = "crab $CRAB_RUNTIME_VERSION"
test "$(/runtime/crab-desktop-agent --version)" = "crab-desktop-agent $CRAB_RUNTIME_VERSION"
mkdir /proof
git init --quiet --template= --initial-branch=main /proof
exec /runtime/crab-desktop-agent`;
  const args = ["run", "--init", "--platform", platform, "--name", name, "--interactive",
    "--volume", `${directory}:/runtime:ro`, "--env", `CRAB_RUNTIME_VERSION=${runtime.version}`,
    "--env", "CRAB_BIN=/runtime/crab", "--env", "GIT_CONFIG_NOSYSTEM=1", "--env", "GIT_CONFIG_GLOBAL=/dev/null",
    image, "/bin/sh", "-ceu", script];
  let operationError;
  try {
    await new Promise((resolve, reject) => {
      const child = spawn("docker", args, { stdio: ["pipe", "pipe", "pipe"] });
      let failure;
      let diagnostics = "";
      let completed = 0;
      let escalation;
      const stop = error => {
        failure ??= error;
        child.kill("SIGTERM");
        escalation ??= setTimeout(() => child.kill("SIGKILL"), 5000);
      };
      const deadline = setTimeout(() => stop(new Error(`${image}: runtime qualification timed out (${name})`)), 180_000);
      child.on("error", stop);
      child.stdin.on("error", stop);
      child.stderr.on("data", bytes => { diagnostics = (diagnostics + bytes.toString()).slice(-64 * 1024); });
      const lines = createInterface({ input: child.stdout });
      const send = () => child.stdin.write(JSON.stringify({ jsonrpc: "2.0", id: completed + 1, method: requests[completed].method, params: requests[completed].params }) + "\n");
      lines.on("line", line => {
        try {
          const frame = JSON.parse(line);
          if (frame.id === undefined) return;
          if (frame.error) throw new Error(`${image}: ${requests[completed]?.method} failed`, { cause: frame.error });
          if (frame.id !== completed + 1 || !requests[completed]?.accepts(frame.result)) throw new Error(`${image}: invalid runtime response`, { cause: frame });
          completed++;
          if (completed === requests.length) child.stdin.end();
          else send();
        } catch (error) { stop(error); }
      });
      child.once("close", code => {
        clearTimeout(deadline); clearTimeout(escalation); lines.close();
        if (failure || code !== 0 || completed !== requests.length) reject(new Error(`${image}: qualification failed (${name})\n${diagnostics}`, { cause: failure }));
        else resolve();
      });
      send();
    });
  } catch (error) { operationError = error; }
  try {
    // The named container owns its cache and processes. Cleanup must finish
    // after both success and failure before this qualification can return.
    await execute("docker", ["rm", "--force", name], { timeout: 15_000, killSignal: "SIGKILL" });
  } catch (cleanup) {
    throw new AggregateError([...(operationError ? [operationError] : []), cleanup], `Linux qualification container cleanup failed: ${name}`);
  }
  if (operationError) throw operationError;
  console.log(JSON.stringify({ image, arch: runtime.arch, version: runtime.version, rpcChecks: requests.length, closed: true }));
}
