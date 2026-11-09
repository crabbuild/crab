import { spawn } from "node:child_process";

const child = spawn(process.execPath, ["node_modules/@playwright/test/cli.js", "test", "--project=e2e-smoke", "--workers=1", "core-v2.spec.ts", "large-files.spec.ts", "large-file-diff.spec.ts", "notebook-diff.spec.ts", "media.spec.ts", "ssh-native.spec.ts", "ssh-host-key.spec.ts"], {
  stdio: "inherit", env: { ...process.env, E2E_SMOKE: "1" },
});
child.once("error", error => { throw error; });
child.once("exit", code => { process.exitCode = code ?? 1; });
