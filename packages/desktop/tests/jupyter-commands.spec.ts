import { expect, it, vi } from "vitest";
const fixture = vi.hoisted(() => ({ environment: vi.fn(), exec: vi.fn() }));
vi.mock("../electron/env/userShellEnv", () => ({ getUserShellBaseEnv: fixture.environment }));
vi.mock("node:child_process", async original => ({ ...await original<typeof import("node:child_process")>(), execFile: fixture.exec }));
vi.mock("electron", () => ({ app: { isPackaged: false } }));
import { checkJupyterRuntime, installJupyter, listKernelSpecs, stopJupyterRuntimeCommands } from "../electron/jupyter-kernel";

it("settles pending runtime discovery on shutdown and never admits a late process", async () => {
  let release!: (env: NodeJS.ProcessEnv) => void;
  fixture.environment.mockReturnValue(new Promise<NodeJS.ProcessEnv>(resolve => { release = resolve; }));
  const check = checkJupyterRuntime("/python");
  const commands = Promise.allSettled([listKernelSpecs("/python"), installJupyter("/python")]);
  await stopJupyterRuntimeCommands();
  expect(await check).toMatchObject({ available: false, error: "Jupyter services are shutting down" });
  expect((await commands).every(result => result.status === "rejected")).toBe(true);
  release({ PATH: "/late" });
  await Promise.resolve();
  expect(fixture.exec).not.toHaveBeenCalled();
});
