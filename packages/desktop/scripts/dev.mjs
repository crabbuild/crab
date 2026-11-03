import { spawn } from "node:child_process";
import { createServer } from "vite";
import electron from "electron";

const server = await createServer();
await server.listen();
server.printUrls();
const child = spawn(electron, ["."], {
  stdio: "inherit",
  env: { ...process.env, VITE_DEV_SERVER_URL: "http://localhost:5173" },
});
child.once("error", async error => { await server.close(); throw error; });
child.once("exit", async code => { await server.close(); process.exitCode = code ?? 1; });
for (const signal of ["SIGINT", "SIGTERM"]) process.once(signal, () => child.kill(signal));
