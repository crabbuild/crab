import { spawn } from "node:child_process";

const child = spawn(process.execPath, ["node_modules/@playwright/test/cli.js", "test", "--project=e2e-smoke", "core-v2.spec.ts"], {
  stdio: "inherit", env: { ...process.env, E2E_SMOKE: "1" },
});
child.once("error", error => { throw error; });
child.once("exit", code => { process.exitCode = code ?? 1; });
