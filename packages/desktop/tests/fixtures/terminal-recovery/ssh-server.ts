import { terminalSshServer } from "../ssh-terminal-server";
import { mkdirSync, writeFileSync } from "node:fs";
import * as path from "node:path";
import { execFileSync } from "node:child_process";
const root = process.argv.at(-1)!;
const remote = path.join(root, "remote");
mkdirSync(remote, { recursive: true });
const server = await terminalSshServer(remote);
writeFileSync(path.join(root, "ssh-connection.json"), JSON.stringify(server.connection));
console.log("Isolated SSH verification server ready");
process.on("SIGTERM", async () => {
  await server.close();
  try { execFileSync("tmux", ["-L", "crab-desktop", "kill-server"], { env: { ...server.env, DYLD_LIBRARY_PATH: process.env.CRAB_TEST_DYLD_LIBRARY_PATH }, stdio: "ignore" }); } catch {}
  process.exit(0);
});
