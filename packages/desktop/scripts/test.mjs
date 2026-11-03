import { spawn } from "node:child_process";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";
import path from "node:path";

// Tests load the same native SQLite/PTY ABI that npm's Electron rebuild installs.
const require = createRequire(import.meta.url);
const executable = require("electron");
const desktop = fileURLToPath(new URL("../", import.meta.url));
const platform = { darwin: "mac", win32: "win", linux: "linux" }[process.platform];
const crab = path.join(desktop, "resources", "native", `${platform}-${process.arch}`, process.platform === "win32" ? "crab.exe" : "crab");
const child = spawn(executable, ["node_modules/vitest/vitest.mjs", "run", ...process.argv.slice(2)], {
  stdio: "inherit", env: { ...process.env, CRAB_BIN: process.env.CRAB_BIN ?? crab, ELECTRON_RUN_AS_NODE: "1" },
});
child.once("error", error => { throw error; });
child.once("exit", code => { process.exitCode = code ?? 1; });
