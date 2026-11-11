import { spawn } from "node:child_process";

const child = spawn(process.execPath, ["node_modules/@playwright/test/cli.js", "test", "--project=e2e-smoke", "--workers=1", "core-v2.spec.ts", "large-files.spec.ts", "large-file-lifecycle.spec.ts", "file-transfer-recovery.spec.ts", "large-file-diff.spec.ts", "notebook-diff.spec.ts", "media.spec.ts", "ssh-native.spec.ts", "ssh-bootstrap.spec.ts", "ssh-bootstrap-ui.spec.ts", "ssh-host-key.spec.ts", "native-recovery.spec.ts", "metadata-recovery-ui.spec.ts", "selected-source-recovery.spec.ts", "repository-open-lifecycle.spec.ts", "ssh-reopen-ui.spec.ts", "ssh-metadata-replay-ui.spec.ts", "storage-identity.spec.ts", "settings-native-unavailable.spec.ts", "settings-actor-construction.spec.ts"], {
  stdio: "inherit", env: { ...process.env, E2E_SMOKE: "1" },
});
child.once("error", error => { throw error; });
child.once("exit", code => { process.exitCode = code ?? 1; });
